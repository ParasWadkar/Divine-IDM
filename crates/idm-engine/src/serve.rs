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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::failure::Failure;
use crate::pairing::{PairingOutcome, PairingStore};
use idm_core::win::power::{ActionOutcome, ArmedAction, Canceller, PostAction, SystemPower};
use idm_ipc::protocol::{
    DownloadRequest, JobId, JobState, JobStatus, PostActionSetting, QueueView, Request, Response,
    Settings,
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
    /// The named queue governing this job, if any.
    ///
    /// Held here as well as in the row because the scheduler reads it on
    /// every handover, and reaching the disk while holding the job lock is
    /// the one thing that loop must not do.
    queue: Option<i64>,
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
    /// The post-download action counting down, if one is.
    ///
    /// More than the canceller, because the window has to be able to say
    /// *what* is about to happen and *when*: a bare "something is armed"
    /// is not something anyone can act on, and for three of the four
    /// actions the thing about to happen is the machine going away.
    armed: Mutex<Option<Armed>>,
    /// The BitTorrent session, started the first time one is needed.
    ///
    /// Lazy on purpose. A session listens on a port, joins the DHT and
    /// announces itself; somebody who never opens a torrent should have this
    /// app doing none of that. One for the whole engine, not one per job:
    /// see `idm_torrent::Torrents`.
    torrents: tokio::sync::OnceCell<Arc<idm_torrent::Torrents>>,
    /// The named queues, mirrored from the store.
    ///
    /// A mirror rather than a lookup: `pump` consults this while holding the
    /// job registry, and a SQLite read under that lock would put the disk on
    /// the path of every single handover. The store stays the durable copy
    /// and this is reloaded from it whenever a queue changes.
    queues: Mutex<Vec<crate::queues::Queue>>,
    /// The bandwidth rules, mirrored from the store.
    ///
    /// A mirror for the same reason the queues are one: `pump` consults it
    /// while holding the job registry, and reaching SQLite under that lock
    /// would put the disk on the path of every handover between downloads.
    rules: Mutex<Vec<crate::schedule::Rule>>,
    /// What the schedule last decided.
    ///
    /// Kept rather than recomputed because `pump` consults it on every
    /// handover, and reading the clock and walking the rules there would
    /// put both on the path of starting every download.
    in_force: Mutex<crate::schedule::Action>,
    /// Where saved passwords live.
    ///
    /// Behind a trait so the tests can prove the refusals without writing
    /// to the machine's real Credential Manager, and without leaving
    /// entries behind in somebody's Control Panel.
    vault: Arc<dyn idm_core::win::credentials::Vault>,
    /// The torrents currently in that session, by job.
    ///
    /// Needed because cancelling aborts the job's task, and a torrent is not
    /// the task -- it lives in the session and would keep running, and keep
    /// talking to peers, with nothing left watching it.
    live_torrents: Mutex<HashMap<JobId, Arc<idm_torrent::Torrent>>>,
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

    /// Folds the database's write-ahead log back into the file itself.
    ///
    /// For callers on their way out. Nothing closes this connection -- a
    /// Tauri `exit` and a `process::exit` both skip every destructor -- so
    /// without this the newest rows stay in the log and `state.sqlite3`
    /// alone, which is what a backup takes, keeps what it last folded back.
    ///
    /// Reported rather than returned: the caller is already leaving, and
    /// there is nothing it could usefully do about a failure.
    pub fn checkpoint(&self) {
        if let Some(store) = &self.inner.store
            && let Err(error) = store.checkpoint()
        {
            tracing::warn!(%error, "could not fold the database log back on the way out");
        }
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
        // Read once, here, rather than on demand. Without this the first
        // handover after a restart would find no queues and start everything
        // a queue was meant to hold back.
        let queues = store.as_deref().map(read_queues).unwrap_or_default();
        // Read here too, and for the same reason: without it the first
        // handover after a restart would find no rules and start downloads
        // a schedule was meant to be holding back.
        let rules = store.as_deref().map(read_rules).unwrap_or_default();
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
                torrents: tokio::sync::OnceCell::new(),
                live_torrents: Mutex::new(HashMap::new()),
                queues: Mutex::new(queues),
                rules: Mutex::new(rules),
                // Whatever the settings say until the schedule task has
                // had its first look, which is the behaviour from before
                // schedules existed.
                in_force: Mutex::new(crate::schedule::Action::Limit(starting_limit)),
                vault: Arc::new(idm_core::win::credentials::WindowsVault),
            }),
        });
        start_stall_watchdog(Arc::clone(&engine.inner));
        start_bandwidth_schedule(Arc::clone(&engine.inner));
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
        // measuring it would refuse every stream there is. A `.torrent` is
        // the same trick in a different wrapper: thirty kilobytes of bencode
        // standing for however large the swarm's files are.
        let manifest = idm_net::stream::looks_like_manifest(&request.url);
        let torrent = idm_torrent::Source::parse(&request.url);
        if let Some(size) = request.size_hint
            && !manifest
            && torrent.is_none()
            && size < min_takeover_bytes
        {
            return Err(format!(
                "{size} bytes is below the {min_takeover_bytes} byte takeover threshold"
            ));
        }

        // A torrent names itself. Taking the leaf of the address would give
        // `download.bin` for every magnet there is, since a magnet has no
        // path to take a leaf from.
        if let Some(source) = &torrent {
            let name = request
                .filename
                .as_deref()
                .and_then(sanitise_filename)
                .or_else(|| sanitise_filename(&source.provisional_name()))
                .unwrap_or_else(|| "torrent".to_owned());
            return Ok(name);
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
                    reason: "browser integration is switched off in DivinePulse".into(),
                };
            }
            // Approved by a human, in the app, before anything is queued.
            let paired = match &client.client_id {
                Some(id) => self.inner.pairings.lock().await.is_paired(id),
                None => false,
            };
            if !paired {
                return Response::Rejected {
                    reason: "this extension is not paired; approve it in DivinePulse".into(),
                };
            }
            // Enforced here, not in the extension, for the same reason as the
            // kill switch above: a switch only the extension honours is
            // cosmetic to anything holding the token.
            let enabled = match &client.client_id {
                Some(id) => self.inner.pairings.lock().await.is_enabled(id),
                None => false,
            };
            if !enabled {
                return Response::Rejected {
                    reason: "this browser is switched off in DivinePulse".into(),
                };
            }
        }

        // Only the app and the command line may name an ftp or sftp
        // address. A page handing one over would put credentials in a URL
        // and reach servers the browser itself could not.
        let filename = match self.vet(&request, settings.min_takeover_bytes, client.is_trusted()) {
            Ok(name) => name,
            Err(reason) => return Response::Rejected { reason },
        };

        // Settled once, because three later decisions turn on it and
        // re-parsing in each place is how they come to disagree.
        let torrent = idm_torrent::Source::parse(&request.url);

        // A name the URL cannot supply is asked of the server before anything
        // is decided. See `name_from_server`. Never for a torrent: there is
        // no server to ask, and a magnet would be sent as an HTTP address.
        // The real name comes from the metadata, once the swarm supplies it.
        let filename =
            if torrent.is_none() && request.filename.is_none() && !has_extension(&filename) {
                // The proxy goes with it: this is a real request to the origin,
                // and a network where downloads must be proxied is one where
                // this probe would otherwise hang until its timeout and then
                // silently fall back to guessing from the URL.
                let proxy =
                    crate::proxy::mode_for(&self.inner.settings.lock().await.clone(), &request.url)
                        .await;
                name_from_server(&request, proxy).await.unwrap_or(filename)
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

        // Checked against the queues that exist, not taken on trust. An id
        // naming nothing would otherwise be kept on the job and read as "no
        // queue" by the scheduler, which starts the download immediately --
        // the opposite of what asking for a queue meant.
        let queue = match request.queue {
            Some(wanted) => {
                let queues = self.inner.queues.lock().await;
                if crate::queues::find(&queues, wanted).is_none() {
                    return Response::Rejected { reason: "that queue no longer exists".into() };
                }
                Some(wanted)
            }
            None => None,
        };

        let id = JobId(self.inner.next_id.fetch_add(1, Ordering::SeqCst));

        // Decided before the download starts, because the destination has
        // to be settled before the first byte is written — there is no
        // moving the file afterwards without racing whatever opened it.
        // Not for a torrent: there is nothing yet to sort by. Its name is an
        // info hash, or a folder name with no extension, and what it
        // actually contains is known only once the metadata arrives. That
        // happens in `run_torrent`, which places it before writing anything
        // -- which is all the rule here ever required.
        let category = (torrent.is_none() && settings.sort_into_folders)
            .then(|| crate::category::category_for(&filename))
            .flatten();
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
        //
        // Not for a torrent. A claim reserves one file path and sidesteps a
        // collision by downloading to `name (1)`; a torrent writes a whole
        // folder of files whose names come from the metadata, and librqbit
        // places them itself. Reserving a path here would reserve one nothing
        // is ever written to.
        let (filename, claimed_path) = if torrent.is_some() {
            (filename, None)
        } else {
            let (filename, claimed) = self.claim_destination(&output_dir, &filename).await;
            (filename, Some(claimed))
        };

        let mut config = DownloadConfig::new(request.url.clone(), output_dir.clone());
        config.filename = Some(filename.clone());
        config.conn.per_host_max = self.inner.max_connections;
        config.referrer = request.referrer.clone();
        config.cookies = request.cookies.clone();
        config.mirrors = request.mirrors.clone();
        config.fetch.limiter = Some(Arc::clone(&self.inner.limiter));
        config.http.proxy =
            crate::proxy::mode_for(&self.inner.settings.lock().await.clone(), &config.url).await;
        self.attach_credential(&mut config).await;
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
                // Kept so that pausing and resuming does not quietly turn a
                // multi-source download into a single-source one.
                mirrors: request.mirrors.clone(),
                ..Default::default()
            };
            match store.add_job(&row) {
                Ok(row_id) => {
                    self.inner.rows.lock().await.insert(id, row_id);
                    // So a restart finds it in the same queue. Written after
                    // the row exists rather than as a column on it, because
                    // the assignment also sets the job's position within the
                    // queue, which is the store's business to work out.
                    if let Some(queue) = queue
                        && let Err(error) = store.assign_to_queue(row_id, Some(queue))
                    {
                        tracing::warn!(%error, "could not put the job in its queue");
                    }
                }
                Err(error) => tracing::warn!(%error, "could not record the job"),
            }
        }

        let status = JobStatus {
            id,
            url: request.url.clone(),
            filename,
            output_dir: output_dir.display().to_string(),
            state: JobState::Queued,
            downloaded: 0,
            total: None,
            bytes_per_sec: 0.0,
            connections: 0,
            error: None,
            retry_in_secs: None,
            note: None,
            error_kind: None,
            queue,
            // Filled in by `status`, from the queue table as it stands when
            // the window asks.
            queue_name: None,
        };

        self.inner.jobs.lock().await.insert(
            id,
            JobRecord {
                status,
                abort: None,
                claimed: claimed_path,
                config: Box::new(config),
                waiting: true,
                queue,
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
                    job.status.error_kind = None;
                    // A fresh attempt vets the extra addresses again, so
                    // whatever it decided last time is not evidence now.
                    job.status.note = None;
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

    /// Starts a failed download again from its History row.
    ///
    /// `resume` above only ever finds the job in `self.inner.jobs`, which is
    /// rebuilt empty on every start from `unfinished_jobs()` alone -- a
    /// `Failed` job is deliberately excluded from that list, so once the
    /// engine that failed it has restarted, there is nothing left in memory
    /// for `resume` to find. The `.idmpart` and its journal are still on
    /// disk regardless, exactly as they are for a paused download, so this
    /// rebuilds the same in-memory record `restore_interrupted` would have
    /// built, directly from the row, and hands it a fresh id.
    async fn resume_history(&self, row_id: i64, fresh_url: Option<String>) -> Response {
        // Vetted exactly as a new download would be, the same way
        // `refresh_url` vets its replacement. A fresh address is no more
        // trustworthy for having been typed into a window.
        if let Some(url) = &fresh_url {
            let probe = DownloadRequest { url: url.clone(), ..Default::default() };
            if let Err(reason) = probe.validate_with(true) {
                return Response::Rejected { reason };
            }
        }

        let Some(store) = self.inner.store.clone() else {
            return Response::Rejected { reason: "there is no download history".into() };
        };

        // Already live under some id -- the ordinary Resume covers that
        // case, and starting a second copy from the same row would race it
        // for the same bytes.
        if self.inner.rows.lock().await.values().any(|&existing| existing == row_id) {
            return Response::Rejected { reason: "that download is already going".into() };
        }

        let row = match store.job(row_id) {
            Ok(Some(row)) => row,
            Ok(None) => return Response::Rejected { reason: "no such download".into() },
            Err(error) => return Response::Error { message: error.to_string() },
        };

        match row.state {
            idm_store::JobState::Completed => {
                return Response::Rejected { reason: "that download has already finished".into() };
            }
            // Both terminal states that left bytes on disk. A cancelled
            // download is as resumable as a failed one -- the files were
            // thrown away, but the user asking again is asking for it to
            // start over, not to be told it cannot.
            idm_store::JobState::Failed | idm_store::JobState::Cancelled => {}
            // Queued, Running or Paused: still live, or the row is stale
            // because the engine has not caught up with it yet either way.
            _ => return Response::Rejected { reason: "that download is already going".into() },
        }

        // The cookies that authorised it, if any, were never written to the
        // database -- see the crate documentation -- so there is nothing to
        // resume with here. Saying so is more useful than a 403.
        if row.needs_browser_auth {
            return Response::Rejected {
                reason: "this download was authorised by your browser; start it there again".into(),
            };
        }

        // The replacement wins from here on, and is written back to the row:
        // a download picked up from History a second time should start from
        // the address that worked, not the one that expired.
        let url = match &fresh_url {
            Some(url) => {
                if let Err(error) = store.set_job_url(row.id, url) {
                    tracing::warn!(%error, "could not record the new address");
                }
                url.clone()
            }
            None => row.url.clone(),
        };

        let id = JobId(self.inner.next_id.fetch_add(1, Ordering::SeqCst));
        let mut config = DownloadConfig::new(url.clone(), PathBuf::from(&row.output_dir));
        config.filename = Some(row.filename.clone());
        config.conn.per_host_max = self.inner.max_connections;
        config.referrer = row.referrer.clone();
        config.mirrors = row.mirrors.clone();
        config.fetch.limiter = Some(Arc::clone(&self.inner.limiter));
        config.http.proxy =
            crate::proxy::mode_for(&self.inner.settings.lock().await.clone(), &config.url).await;
        self.attach_credential(&mut config).await;
        config.profiles =
            self.inner.store.clone().map(|store| store as Arc<dyn idm_core::sched::ProfileStore>);

        let destination = PathBuf::from(&row.output_dir).join(&row.filename);
        // Whatever queue it was in before, so starting a download again from
        // History does not quietly promote it past everything in that queue.
        let queue = row.queue_id;
        let status = JobStatus {
            id,
            url: url.clone(),
            filename: row.filename.clone(),
            output_dir: row.output_dir.clone(),
            state: JobState::Queued,
            downloaded: row.downloaded,
            total: row.total,
            bytes_per_sec: 0.0,
            connections: 0,
            error: None,
            retry_in_secs: None,
            note: None,
            error_kind: None,
            queue,
            // Filled in by `status`, from the queue table as it stands when
            // the window asks.
            queue_name: None,
        };

        self.inner.claimed.lock().await.insert(destination.clone());
        self.inner.rows.lock().await.insert(id, row.id);
        self.inner.jobs.lock().await.insert(
            id,
            JobRecord {
                status,
                abort: None,
                claimed: Some(destination),
                config: Box::new(config),
                waiting: true,
                queue,
                attempts: 0,
                last_progress: std::time::Instant::now(),
            },
        );

        write_state(&self.inner, id, JobState::Queued, None).await;
        pump(Arc::clone(&self.inner)).await;
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
            config.mirrors = row.mirrors.clone();
            config.fetch.limiter = Some(Arc::clone(&self.inner.limiter));
            config.http.proxy =
                crate::proxy::mode_for(&self.inner.settings.lock().await.clone(), &config.url)
                    .await;
            self.attach_credential(&mut config).await;
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
                output_dir: row.output_dir.clone(),
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
                note: None,
                error_kind: None,
                queue: row.queue_id,
                queue_name: None,
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
                    queue: row.queue_id,
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
        // Read before the job lock, not under it, and kept in the order the
        // two locks are taken everywhere else in this file.
        let queues = self.inner.queues.lock().await.clone();

        let jobs = self.inner.jobs.lock().await;
        let mut listed: Vec<JobStatus> = match id {
            Some(wanted) => jobs.get(&wanted).map(|job| job.status.clone()).into_iter().collect(),
            None => jobs.values().map(|job| job.status.clone()).collect(),
        };
        // Filled in here rather than stored on the job, so renaming a queue
        // shows up on everything in it without touching a single record.
        for job in &mut listed {
            job.queue_name = job
                .queue
                .and_then(|id| crate::queues::find(&queues, id))
                .map(|queue| queue.name.clone());
        }
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
                    job.status.error_kind = None;
                    // Stopped by hand, so it is not waiting for a slot; it
                    // waits for the user instead.
                    job.waiting = false;
                    job.attempts = 0;
                    Response::Accepted { id }
                }
            }
        };
        // Aborting the task is enough to stop every other kind of download,
        // because the task *is* the download. A torrent is not: it lives in
        // the session and would carry on talking to peers at full speed
        // behind a list that says Paused. Paused rather than removed, so
        // librqbit keeps its record of which pieces are verified.
        if let Some(torrent) = self.inner.live_torrents.lock().await.get(&id).cloned()
            && let Err(error) = torrent.pause().await
        {
            tracing::warn!(%error, "could not pause a torrent");
        }

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
    async fn refresh_url(
        &self,
        id: JobId,
        url: String,
        referrer: Option<String>,
        cookies: Option<String>,
    ) -> Response {
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
                    // Only what was actually given. A caller that pasted an
                    // address and nothing else means "the headers are still
                    // good", and clearing them would turn one expired thing
                    // into two.
                    if let Some(referrer) = referrer {
                        job.config.referrer = Some(referrer);
                    }
                    if let Some(cookies) = cookies {
                        job.config.cookies = Some(cookies);
                    }
                    job.status.url = url;
                    job.status.state = JobState::Queued;
                    job.status.error = None;
                    job.status.error_kind = None;
                    job.status.retry_in_secs = None;
                    // Said about the address that has just been replaced.
                    job.status.note = None;
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
                    record.status.state = JobState::Cancelled;
                    // Nothing went wrong, so there is nothing to report. The
                    // state is the whole story.
                    record.status.error = None;
                    record.status.error_kind = None;
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

        // Out of the session altogether, so it stops talking to peers and
        // stops being resumed by anything later.
        //
        // The files are kept, which is where this parts company with
        // cancelling any other download: above, the part file and journal are
        // deleted, because they are scratch work nothing else could want. A
        // torrent writes its real files in their real place, under names that
        // came from the metadata -- and, because resuming requires writing
        // into files that already exist, some of those files may have been
        // there before this job and belong to somebody else. Deleting them on
        // a cancel is not a risk worth taking to save some disk.
        if let Some(torrent) = self.inner.live_torrents.lock().await.remove(&id)
            && let Err(error) = torrent.remove(false).await
        {
            tracing::warn!(%error, "could not remove a cancelled torrent from the session");
        }

        write_state(&self.inner, id, JobState::Cancelled, None).await;
        release_claim(&self.inner, id).await;
        // The download task drops this mapping itself once it stops, but it
        // was aborted above and never reached that line. Left behind, the row
        // still looks live for the rest of the process, and History's Resume
        // refuses to start a download it believes is already going. After
        // `write_state`, which needs the mapping to find the row.
        forget_row(&self.inner, id).await;
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
/// What the limiter should be set to, and whether anything may run.
///
/// The one place the precedence lives. A rule in force wins over the
/// engine-wide limit rather than being combined with it, which is what
/// makes "a megabyte a second normally, full speed overnight" expressible;
/// taking the lower of the two would silently make it impossible. With no
/// rule in force the engine-wide limit applies unchanged.
async fn effective_action(inner: &Arc<Inner>) -> crate::schedule::Action {
    let rules = inner.rules.lock().await.clone();
    match crate::schedule::in_force(crate::schedule::now(), &rules) {
        Some(action) => action,
        None => {
            let limit = inner.settings.lock().await.speed_limit_bytes_per_sec;
            crate::schedule::Action::Limit(limit)
        }
    }
}

/// Puts the schedule's answer into effect, and says whether it changed.
///
/// A stop window is a limit of nothing *and* a gate on starting anything;
/// the limiter is left alone for it, because the downloads it would apply
/// to are being taken off the road entirely.
async fn apply_schedule(inner: &Arc<Inner>) -> bool {
    let action = effective_action(inner).await;
    let changed = {
        // One hold, not two readings: what was in force and what now is
        // have to be decided together, or two of these racing could each
        // see the other's answer as the previous one and both report no
        // change.
        let mut in_force = inner.in_force.lock().await;
        let was = std::mem::replace(&mut *in_force, action);
        was != action
    };

    if let crate::schedule::Action::Limit(bytes) = action {
        inner.limiter.set_rate(bytes).await;
    }
    changed
}

/// Keeps the speed limit in step with the time of day.
///
/// Wakes at each boundary rather than polling, because a schedule's
/// boundaries are minutes apart at the closest and days apart at the
/// furthest. The wait is capped all the same: a laptop that slept through
/// a boundary, a clock put right by hand and the night the clocks go
/// forward all move local time underneath a sleeping task, and a cap is
/// what notices.
fn start_bandwidth_schedule(inner: Arc<Inner>) {
    // As the watchdog: an engine built outside a runtime has nothing to
    // schedule, and spawning there would panic.
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }

    tokio::spawn(async move {
        /// Long enough not to be a poll, short enough that a clock which
        /// jumped is noticed before anybody would.
        const LONGEST_SLEEP: u64 = 10 * 60;

        loop {
            let changed = apply_schedule(&inner).await;
            if changed {
                // Only on a change, so a download is not torn down and put
                // back every ten minutes for the whole of a stop window.
                enforce_stop(&inner).await;
                pump(Arc::clone(&inner)).await;
            }

            let rules = inner.rules.lock().await.clone();
            let ahead = crate::schedule::next_change(crate::schedule::now(), &rules)
                .map_or(LONGEST_SLEEP, |minutes| u64::from(minutes) * 60);
            tokio::time::sleep(std::time::Duration::from_secs(ahead.min(LONGEST_SLEEP))).await;
        }
    });
}

/// Takes running downloads off the road when a stop window begins.
///
/// They go back to being queued and waiting, which is where they were
/// before the scheduler handed them a slot -- not paused. Paused is the
/// user's word: a download the schedule had paused would be
/// indistinguishable from one they paused themselves, so closing the app
/// inside a window would leave downloads stopped with nobody to start them
/// again. A queued one simply waits, and `pump` starts it when the window
/// ends.
///
/// Aborting mid-transfer is the established move here -- the stall
/// watchdog and the retry path both do it -- and the journal keeps the
/// bytes, so what resumes carries on rather than starting over.
async fn enforce_stop(inner: &Arc<Inner>) {
    if *inner.in_force.lock().await != crate::schedule::Action::Stop {
        return;
    }

    let running: Vec<JobId> = {
        let jobs = inner.jobs.lock().await;
        jobs.iter()
            .filter(|(_, job)| job.status.state == JobState::Running)
            .map(|(id, _)| *id)
            .collect()
    };

    for id in running {
        let mut jobs = inner.jobs.lock().await;
        let Some(job) = jobs.get_mut(&id) else { continue };
        if let Some(abort) = job.abort.take() {
            abort.abort();
        }
        job.status.state = JobState::Queued;
        job.status.bytes_per_sec = 0.0;
        job.status.connections = 0;
        // Back to where `pump` found it, so the window ending starts it
        // again with no further bookkeeping.
        job.waiting = true;
    }
}

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
                schedule_retry(&inner, id, Failure::stalled()).await;
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
async fn schedule_retry(inner: &Arc<Inner>, id: JobId, failure: Failure) {
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
        job.status.error = Some(failure.summary.clone());
        job.status.error_kind = Some(failure.kind.code().to_owned());
        job.waiting = false;
        job.abort = None;
        delay
    };

    // Recorded even though the row is not finished: a download waiting to
    // try again is one the user may ask about, and "why" is the question.
    write_state(inner, id, JobState::Reconnecting, Some(outcome_of(&failure))).await;
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
async fn name_from_server(
    request: &DownloadRequest,
    proxy: idm_net::client::ProxyMode,
) -> Option<String> {
    let mut config = idm_net::download::DownloadConfig::new(request.url.clone(), PathBuf::new());
    config.referrer = request.referrer.clone();
    config.cookies = request.cookies.clone();
    config.http.proxy = proxy;

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
) -> std::result::Result<Option<u64>, Failure> {
    // A place for the download to say what it decided quietly. Read by a
    // task of its own rather than from the progress callback, because that
    // one only ever gets the job lock on a `try_lock` -- right for a figure
    // that arrives again in two seconds, wrong for something said once.
    let (notes, mut incoming) = tokio::sync::mpsc::unbounded_channel::<String>();
    let notes_inner = Arc::clone(inner);
    let reader = tokio::spawn(async move {
        while let Some(note) = incoming.recv().await {
            let mut jobs = notes_inner.jobs.lock().await;
            if let Some(record) = jobs.get_mut(&id) {
                record.status.note = Some(note);
            }
        }
    });

    let mut config = config.clone();
    config.notes = Some(notes);

    let progress_inner = Arc::clone(inner);
    let report = download_with_progress(&config, move |progress: Progress| {
        record_progress(&progress_inner, id, &progress);
    })
    .await;

    // Dropping the config drops the sender, which ends the reader.
    drop(config);
    let _ = reader.await;
    let report = report?;

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

/// The BitTorrent session, started on first use.
///
/// `OnceCell` rather than a field set at construction, so that an app nobody
/// torrents with never opens a listening socket or joins the DHT.
async fn torrent_session(
    inner: &Arc<Inner>,
) -> std::result::Result<Arc<idm_torrent::Torrents>, Failure> {
    // Read before the session starts and not re-read afterwards: librqbit
    // takes its rate limit when the session is built. A limit changed later
    // applies to the next run of the app, which is worth saying out loud
    // rather than pretending otherwise.
    let limit = inner.settings.lock().await.speed_limit_bytes_per_sec;
    let download_dir = inner.output_dir.clone();

    inner
        .torrents
        .get_or_try_init(|| async {
            idm_torrent::Torrents::start(&idm_ipc::state_dir(), download_dir, limit)
                .await
                .map(Arc::new)
                .map_err(Failure::from_transport)
        })
        .await
        .cloned()
}

/// How often a running torrent's statistics are read.
///
/// Slower than the window's own refresh. There is nothing here to interpolate
/// and a torrent's rate is already an average over seconds, so polling faster
/// would only cost locks.
const TORRENT_POLL: std::time::Duration = std::time::Duration::from_millis(500);

/// A torrent, from a magnet link or a `.torrent`.
///
/// Unlike every other driver here this one does not fetch anything itself: it
/// hands the address to librqbit's session and then watches. The loop exists
/// for progress, not for transfer.
async fn run_torrent(
    inner: &Arc<Inner>,
    id: JobId,
    config: &DownloadConfig,
    source: idm_torrent::Source,
) -> std::result::Result<Option<u64>, Failure> {
    // A torrent this job already has is one that was paused: librqbit kept
    // it, along with the record of which pieces are verified, so resuming is
    // unpausing rather than adding it again. Adding again would throw that
    // record away and re-check the files on disk.
    let existing = inner.live_torrents.lock().await.get(&id).cloned();
    let torrent = match existing {
        Some(torrent) => {
            torrent.resume().await.map_err(Failure::from_transport)?;
            torrent
        }
        None => {
            let session = torrent_session(inner).await?;

            // Read before it is started, so the destination can be settled
            // before the first byte -- the same rule every other download
            // here follows. For a magnet this is a swarm lookup and the job
            // sits in `Preparing` while it happens; starting from the
            // listing costs no second lookup.
            let listing = session.inspect(&source).await.map_err(Failure::from_transport)?;
            let output_dir = place_torrent(inner, config, &listing).await?;

            let torrent = Arc::new(
                session
                    .start_listed(&listing, &output_dir)
                    .await
                    .map_err(Failure::from_transport)?,
            );
            adopt_torrent_destination(inner, id, &listing.name, &output_dir).await;
            inner.live_torrents.lock().await.insert(id, Arc::clone(&torrent));
            torrent
        }
    };

    let mut ticker = tokio::time::interval(TORRENT_POLL);

    loop {
        ticker.tick().await;

        let progress = torrent.progress();
        record_torrent(inner, id, &progress);

        match &progress.phase {
            idm_torrent::Phase::Finished => {
                // Stops talking to peers. See `stop_seeding` for why this app
                // does not keep seeding on somebody's connection unasked.
                torrent.stop_seeding().await;
                inner.live_torrents.lock().await.remove(&id);
                return Ok(progress.total);
            }
            idm_torrent::Phase::Failed(reason) => {
                inner.live_torrents.lock().await.remove(&id);
                return Err(Failure::from_transport(reason));
            }
            // `Preparing` is fetching metadata or checking files on disk,
            // `Paused` can only be seen here if something outside this loop
            // paused it, and neither is a reason to stop watching.
            _ => {}
        }
    }
}

/// Which folder a torrent's files belong in.
///
/// The category question, asked at the only moment it can be answered: after
/// the metadata and before the first write. Judged by the largest file in the
/// torrent, because the torrent's own name is a folder name with no extension
/// and the smallest file is some subtitles.
async fn place_torrent(
    inner: &Arc<Inner>,
    config: &DownloadConfig,
    listing: &idm_torrent::Listing,
) -> std::result::Result<PathBuf, Failure> {
    // Read now rather than remembered from when the job was queued, which may
    // have been before the setting was last changed.
    if !inner.settings.lock().await.sort_into_folders {
        return Ok(config.output_dir.clone());
    }

    // A job being restarted already has its category in the folder it was
    // given, and joining another one would make `Downloads/Video/Video`.
    let base = base_of(&config.output_dir);
    let name = listing.principal_file.as_deref().unwrap_or(&listing.name);
    let Some(category) = crate::category::category_for(name) else {
        return Ok(base);
    };

    let chosen = base.join(category);
    // Created here, because librqbit is about to be told to write into it and
    // would fail less clearly.
    std::fs::create_dir_all(&chosen).map_err(Failure::from_transport)?;
    Ok(chosen)
}

/// A download folder with any category folder taken back off.
///
/// So that placing a torrent is idempotent: a job resumed from its stored row
/// arrives with the folder it was already given, and this is what makes
/// deciding again produce the same answer rather than a deeper one.
fn base_of(dir: &Path) -> PathBuf {
    let is_category = dir
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| crate::category::all_categories().contains(&name));

    match (is_category, dir.parent()) {
        (true, Some(parent)) => parent.to_owned(),
        _ => dir.to_owned(),
    }
}

/// Records what a torrent turned out to be: its name, and where it went.
///
/// In the list and in the stored row. Without this the history would keep the
/// placeholder -- an info hash -- for good, and Open folder would point at
/// the folder the job was queued with rather than the one it was placed in.
async fn adopt_torrent_destination(inner: &Arc<Inner>, id: JobId, name: &str, dir: &Path) {
    let Some(name) = sanitise_filename(name) else { return };
    let dir = dir.display().to_string();

    if let Some(record) = inner.jobs.lock().await.get_mut(&id) {
        record.status.filename = name.clone();
        record.status.output_dir = dir.clone();
    }

    if let Some((store, row)) = row_of(inner, id).await
        && let Err(error) = store.set_job_destination(row, &name, &dir)
    {
        tracing::warn!(%error, "could not record where a torrent went");
    }
}

/// A torrent's statistics, in the fields the window already draws.
fn record_torrent(inner: &Inner, id: JobId, progress: &idm_torrent::Progress) {
    if let Ok(mut jobs) = inner.jobs.try_lock()
        && let Some(record) = jobs.get_mut(&id)
    {
        record.status.downloaded = progress.downloaded;
        if progress.total.is_some() {
            record.status.total = progress.total;
        }
        record.status.bytes_per_sec = progress.bytes_per_sec;
        // Peers, which is not the same quantity as everywhere else on this
        // field: nothing here chose the number. The swarm did.
        record.status.connections = progress.peers;
    }
}

/// FTP or FTPS. Credentials, if any, come from the address; otherwise the
/// login is anonymous, which is what a bare ftp:// means.
async fn run_ftp(
    inner: &Arc<Inner>,
    id: JobId,
    config: &DownloadConfig,
) -> std::result::Result<Option<u64>, Failure> {
    let mut options = idm_net::ftp::FtpOptions::new(config.output_dir.clone());
    options.filename = config.filename.clone();
    options.limiter = config.fetch.limiter.clone();

    let progress_inner = Arc::clone(inner);
    let report = idm_net::ftp::download(&config.url, &options, move |done, total| {
        record_transfer(&progress_inner, id, done, total);
    })
    .await
    .map_err(Failure::from_transport)?;
    Ok(Some(report.bytes))
}

/// SFTP. There is nobody to type a password into, so this authenticates the
/// way `ssh` does by default — with the keys in ~/.ssh — and checks the host
/// against known_hosts, refusing one it has not seen.
async fn run_sftp(
    inner: &Arc<Inner>,
    id: JobId,
    config: &DownloadConfig,
) -> std::result::Result<Option<u64>, Failure> {
    let mut options = idm_net::sftp::SftpOptions::new(config.output_dir.clone());
    options.filename = config.filename.clone();
    options.limiter = config.fetch.limiter.clone();
    options.use_default_identities = true;

    let progress_inner = Arc::clone(inner);
    let report = idm_net::sftp::download(&config.url, &options, move |done, total| {
        record_transfer(&progress_inner, id, done, total);
    })
    .await
    .map_err(Failure::from_transport)?;
    Ok(Some(report.bytes))
}

/// A stream: manifest, rendition, segments, one file.
///
/// Progress is counted in segments rather than in a total that is not known
/// until the last one arrives. The job's byte count still moves, so the
/// window shows something advancing, but its total stays unset rather than
/// being invented from a guess at the segment size.
/// Whether a failed download is the kind yt-dlp might rescue.
///
/// One failure and one scheme, deliberately narrow. "No length, nothing to
/// fetch in parallel" is what a video page produces and is not what a
/// server being down, a 404, or a full disk produces -- none of which
/// yt-dlp would improve, and all of which it would slow down by being
/// asked. The scheme matters because yt-dlp speaks http and this engine
/// also accepts ftp, sftp, magnet and file addresses it has its own
/// answers for.
fn worth_asking_yt_dlp(kind: crate::failure::Kind, url: &str) -> bool {
    kind == crate::failure::Kind::DownloadUnknownSize
        && (url.starts_with("http://") || url.starts_with("https://"))
}

/// Fetches the video a page is about, with yt-dlp.
///
/// `Err(None)` means yt-dlp is not installed, which is not its failure and
/// must not replace the refusal the ordinary path already gave.
async fn run_video(
    inner: &Arc<Inner>,
    id: JobId,
    config: &DownloadConfig,
) -> std::result::Result<Option<u64>, Option<Failure>> {
    let program = crate::video::find(&crate::sidecars::dir()).map_err(|_| None)?;

    let wanted = crate::video::Wanted {
        url: config.url.clone(),
        output_dir: config.output_dir.clone(),
        filename: config.filename.clone(),
        // The page is what yt-dlp has to read, and these are what made the
        // page readable. The engine has had both all along; this path threw
        // them away, so a video behind a sign-in failed with whatever the
        // site shows a stranger.
        referrer: config.referrer.clone(),
        cookies: config.cookies.clone(),
        // Resolved by this engine's own search, which prefers a trusted
        // copy over whatever is on PATH, rather than leaving yt-dlp to the
        // reverse precedence.
        ffmpeg: idm_media::sidecar::find(
            idm_media::sidecar::Tool::Ffmpeg,
            Some(&crate::sidecars::dir()),
        ),
    };

    let progress_inner = Arc::clone(inner);
    let fetched = crate::video::fetch_with(
        &program,
        &wanted,
        move |progress| {
            if let Ok(mut jobs) = progress_inner.jobs.try_lock()
                && let Some(record) = jobs.get_mut(&id)
            {
                record.status.downloaded = progress.downloaded;
                // Only when it is known. A live stream has no size, and a
                // bar drawn against a total nobody measured is a lie that
                // reaches 100% and keeps going.
                if let Some(total) = progress.total {
                    record.status.total = Some(total);
                }
                record.status.connections = 1;
            }
        },
    )
    .await
    // Not `from_transport`: yt-dlp refusing an address is a verdict, not a
    // blip, and retrying it respawns the whole program every few seconds
    // to be told the same thing.
    .map_err(|error| Some(Failure::from_tool(error)))?;

    // The name comes from the video's title, so the one guessed from the
    // address at enqueue is wrong by the time this finishes -- and it is
    // the name every later step uses to find the file.
    if let Some(name) = fetched.path.file_name().and_then(|n| n.to_str()) {
        let mut jobs = inner.jobs.lock().await;
        if let Some(record) = jobs.get_mut(&id) {
            record.status.filename = name.to_owned();
            record.status.downloaded = fetched.bytes;
            record.status.total = Some(fetched.bytes);
        }
    }
    Ok(Some(fetched.bytes))
}

async fn run_stream(
    inner: &Arc<Inner>,
    id: JobId,
    config: &DownloadConfig,
) -> std::result::Result<Option<u64>, Failure> {
    let mut options = idm_net::stream::StreamOptions::new(config.output_dir.clone());
    options.filename = config.filename.clone();
    options.referrer = config.referrer.clone();
    options.cookies = config.cookies.clone();
    options.limiter = config.fetch.limiter.clone();
    // Where `Request::FetchSidecar` installs ffmpeg. Without this the fetch
    // would succeed and change nothing: the copy would sit in the state
    // folder while every mux went on reporting ffmpeg missing.
    options.sidecar_dir = Some(crate::sidecars::dir());

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
    .map_err(Failure::from_transport)?;

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

    // Nothing starts inside a stop window. Read once here rather than per
    // pass: the window cannot end while this loop is running, and the task
    // that ends it calls `pump` again.
    if *inner.in_force.lock().await == crate::schedule::Action::Stop {
        return;
    }

    loop {
        // Read before the job registry is locked, never under it. The
        // queue table is small and changes rarely; reaching the disk for it
        // while holding the registry would put SQLite on the path of every
        // handover between downloads.
        let queues = inner.queues.lock().await.clone();

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
                    !job.waiting && matches!(job.status.state, JobState::Queued | JobState::Running)
                })
                .count() as u32;
            if outstanding >= limit {
                return;
            }

            // The same count again, per queue, so each queue's own limit can
            // be applied to it. Counted the same way and in the same pass
            // for the same reason.
            let mut busy: HashMap<i64, u32> = HashMap::new();
            for job in jobs.values() {
                let held = !job.waiting
                    && matches!(job.status.state, JobState::Queued | JobState::Running);
                if let (true, Some(queue)) = (held, job.queue) {
                    *busy.entry(queue).or_default() += 1;
                }
            }

            // Oldest first: ids are handed out in order, so that is the
            // order they were asked for in.
            let mut waiting: Vec<(JobId, Option<i64>)> = jobs
                .iter()
                .filter(|(_, job)| job.waiting && job.status.state == JobState::Queued)
                .map(|(id, job)| (*id, job.queue))
                .collect();
            waiting.sort_by_key(|(id, _)| id.0);

            // The first that *may* start, which is not always the first
            // waiting: see `queues::next_to_start`.
            match crate::queues::next_to_start(&waiting, &queues, &busy) {
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
            let outcome = if let Some(source) = idm_torrent::Source::parse(&config.url) {
                run_torrent(&inner, id, &config, source).await
            } else if idm_net::stream::looks_like_manifest(&config.url) {
                run_stream(&inner, id, &config).await
            } else if idm_net::ftp::handles(&config.url) {
                run_ftp(&inner, id, &config).await
            } else if idm_net::sftp::handles(&config.url) {
                run_sftp(&inner, id, &config).await
            } else {
                run_file(&inner, id, &config).await
            };

            // A page is not a file, and the refusal above says so exactly:
            // no length, nothing to fetch in parallel. What the person
            // wanted is the video the page is about, and working out which
            // of a site's streams that is -- for a thousand sites that each
            // change quarterly -- is yt-dlp's whole job.
            //
            // Only after the ordinary path has tried and only on that one
            // failure, so a plain download never pays for this; and only
            // when yt-dlp has already been fetched, deliberately, in
            // Settings. When it is absent the original refusal stands,
            // because "unable to determine file size" is the true answer
            // and installing a program uninvited is not an answer at all.
            let outcome = match outcome {
                Err(failure) if worth_asking_yt_dlp(failure.kind, &config.url) => {
                    match run_video(&inner, id, &config).await {
                        Ok(total) => Ok(total),
                        // Its refusal when it had a go, ours when it was
                        // never there to have one.
                        Err(Some(theirs)) => Err(theirs),
                        Err(None) => Err(failure),
                    }
                }
                other => other,
            };

            match outcome {
                Ok(total) => {
                    // Everything that happens to a file after the bytes are
                    // all there. A step may refuse to deliver the download;
                    // a step that merely could not run does not.
                    match after_the_bytes(&inner, id).await {
                        None => finish(&inner, id, JobState::Completed, total, None).await,
                        Some(refusal) => {
                            // Failed with the file left exactly where it is.
                            // Nothing here deletes anything.
                            finish(&inner, id, JobState::Failed, total, Some(refusal)).await;
                        }
                    }
                }
                Err(failure) => {
                    // The partial file and journal are deliberately left in
                    // place, so a retry resumes rather than restarts.
                    if failure.transient {
                        schedule_retry(&inner, id, failure).await;
                    } else {
                        finish(&inner, id, JobState::Failed, None, Some(failure)).await;
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
                tokio::spawn(run_post_action(armed, all_succeeded, inner.store.clone()));
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

async fn write_state(
    inner: &Inner,
    id: JobId,
    state: JobState,
    outcome: Option<idm_store::Outcome<'_>>,
) {
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
        JobState::Cancelled => idm_store::JobState::Cancelled,
    };
    if let Err(error) = store.set_job_state(row, stored, outcome) {
        tracing::warn!(%error, "could not record the job state");
    }
}

/// The three parts of a failure, borrowed for the write.
fn outcome_of(failure: &Failure) -> idm_store::Outcome<'_> {
    idm_store::Outcome {
        kind: failure.kind.code(),
        summary: &failure.summary,
        detail: &failure.detail,
    }
}

/// Drops the mapping once a job has finished, so the table does not grow for
/// the life of the process. The row itself stays; it is the history.
async fn forget_row(inner: &Inner, id: JobId) {
    inner.rows.lock().await.remove(&id);
}

/// Runs the after-the-bytes steps over a job's finished file.
///
/// Reads the path from the record rather than from the config, because the
/// filename may have been settled after the job was queued -- a server's
/// own name, or a `(2)` added to avoid overwriting something.
async fn after_the_bytes(inner: &Arc<Inner>, id: JobId) -> Option<Failure> {
    let settings = inner.settings.lock().await.clone();
    let path = {
        let jobs = inner.jobs.lock().await;
        let record = jobs.get(&id)?;
        PathBuf::from(&record.status.output_dir).join(&record.status.filename)
    };

    let (report, refusal) = crate::finish::run(&settings, &inner.output_dir, &path).await;
    if let Some(verdict) = &report.scan {
        tracing::debug!(?verdict, path = %path.display(), "scanned a finished download");
    }
    if let Some(unpacked) = &report.unpacked {
        tracing::debug!(?unpacked, path = %path.display(), "unpacked a finished archive");
    }
    if let Some(ran) = &report.hook {
        tracing::debug!(?ran, path = %path.display(), "ran the post-download hook");
    }
    refusal
}

async fn finish(
    inner: &Inner,
    id: JobId,
    state: JobState,
    total: Option<u64>,
    failure: Option<Failure>,
) {
    if let Some(record) = inner.jobs.lock().await.get_mut(&id) {
        record.status.state = state;
        // The short line, not the whole explanation: the window has one row
        // of space for this, and the full text is kept in the store for
        // History's Technical details to open.
        record.status.error = failure.as_ref().map(|failure| failure.summary.clone());
        record.status.error_kind = failure.as_ref().map(|failure| failure.kind.code().to_owned());
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
    write_state(inner, id, state, failure.as_ref().map(outcome_of)).await;
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
            // Said as a refusal on purpose. The extension's greeting is also
            // its status check, and "approved" here would have it report
            // connected while every download it handed over was declined.
            PairingOutcome::SwitchedOff => {
                Response::Rejected { reason: "this browser is switched off in DivinePulse".into() }
            }
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

    /// Attaches the saved password for this download's host, if there is one.
    ///
    /// The same `Arc` goes to both halves of the config, and that is the
    /// point rather than an economy: `http` is used by the probe and the
    /// mirror check, `fetch` by every lane, and a Digest nonce count has to
    /// be one counter across all of them. Two would hand out repeats and a
    /// correct server would start refusing them under load.
    ///
    /// Nothing is read before this, so a download from a host with no saved
    /// password carries no credential at all rather than an empty one.
    async fn attach_credential(&self, config: &mut idm_net::download::DownloadConfig) {
        // The proxy's password first, because it is a different secret from
        // the site's and is wanted even when the site asks for nothing: the
        // proxy stands between this machine and every download, so a 407
        // stops a transfer that the origin would have served anonymously.
        if let idm_net::client::ProxyMode::Manual(address) = &config.http.proxy
            && let Some(found) = crate::credentials::for_proxy(self.inner.vault.as_ref(), address)
        {
            config.http.proxy_auth =
                Some(idm_net::auth::HttpCredential::new(found.username, found.secret.expose()));
        }

        let Some(found) = crate::credentials::for_url(self.inner.vault.as_ref(), &config.url)
        else {
            return;
        };
        let state = Arc::new(idm_net::auth::AuthState::new(idm_net::auth::HttpCredential::new(
            found.username,
            found.secret.expose(),
        )));
        config.http.auth = Some(Arc::clone(&state));
        config.fetch.auth = Some(state);
    }

    async fn list_pairings(&self) -> Response {
        let mut store = self.inner.pairings.lock().await;
        Response::Pairings { paired: store.paired(), pending: store.pending() }
    }

    async fn approve_pairing(&self, request_id: String) -> Response {
        match self.inner.pairings.lock().await.approve(&request_id) {
            Some(paired) => {
                eprintln!("Paired {:?}.", paired.label);
                Response::PairingApproved
            }
            None => Response::Rejected { reason: "no such pairing request".into() },
        }
    }

    async fn deny_pairing(&self, request_id: String) -> Response {
        if self.inner.pairings.lock().await.deny(&request_id) {
            Response::Pong
        } else {
            Response::Rejected { reason: "no such pairing request".into() }
        }
    }

    async fn revoke_pairing(&self, client_id: String) -> Response {
        if self.inner.pairings.lock().await.revoke(&client_id) {
            Response::Pong
        } else {
            Response::Rejected { reason: "that client was not paired".into() }
        }
    }

    async fn set_browser_enabled(&self, client_id: String, enabled: bool) -> Response {
        if self.inner.pairings.lock().await.set_enabled(&client_id, enabled) {
            Response::Pong
        } else {
            Response::Rejected { reason: "that client was not paired".into() }
        }
    }

    /// The queues, with what each is running and holding right now.
    async fn list_queues(&self) -> Response {
        let queues = self.inner.queues.lock().await.clone();
        let jobs = self.inner.jobs.lock().await;

        let mut running: HashMap<i64, u32> = HashMap::new();
        let mut waiting: HashMap<i64, u32> = HashMap::new();
        for job in jobs.values() {
            let Some(queue) = job.queue else { continue };
            // Counted the way `pump` counts, so the window's numbers and the
            // scheduler's cannot disagree: a job that has been handed a slot
            // is running even in the moment before its task says so.
            match (job.waiting, job.status.state) {
                (false, JobState::Queued | JobState::Running) => {
                    *running.entry(queue).or_default() += 1;
                }
                (true, JobState::Queued) => *waiting.entry(queue).or_default() += 1,
                _ => {}
            }
        }
        drop(jobs);

        Response::Queues {
            queues: queues
                .into_iter()
                .map(|queue| QueueView {
                    running: running.get(&queue.id).copied().unwrap_or(0),
                    waiting: waiting.get(&queue.id).copied().unwrap_or(0),
                    id: queue.id,
                    name: queue.name,
                    max_concurrent: queue.max_concurrent,
                    enabled: queue.enabled,
                })
                .collect(),
        }
    }

    /// Creates a queue, or changes the concurrency of the one with that name.
    async fn set_queue(&self, name: String, max_concurrent: u32) -> Response {
        let Some(store) = self.inner.store.clone() else {
            return Response::Rejected { reason: NO_STORE.into() };
        };
        let name = match crate::queues::vet_name(&name) {
            Ok(name) => name,
            Err(reason) => return Response::Rejected { reason },
        };
        let max_concurrent = match crate::queues::vet_limit(max_concurrent) {
            Ok(limit) => limit,
            Err(reason) => return Response::Rejected { reason },
        };

        match store.upsert_queue(&name, max_concurrent) {
            Ok(_) => {
                self.reload_queues().await;
                // Raising a limit means there may be room now that there was
                // not a moment ago, and nothing else would notice: a slot
                // opening is normally a download finishing.
                pump(Arc::clone(&self.inner)).await;
                self.list_queues().await
            }
            Err(error) => Response::Rejected { reason: error.to_string() },
        }
    }

    async fn enable_queue(&self, id: i64, enabled: bool) -> Response {
        let Some(store) = self.inner.store.clone() else {
            return Response::Rejected { reason: NO_STORE.into() };
        };
        if let Err(error) = store.set_queue_enabled(id, enabled) {
            return Response::Rejected { reason: error.to_string() };
        }
        self.reload_queues().await;
        // Switching a queue off stops it starting anything more; it
        // deliberately does not stop what is already running. Stopping a
        // download mid-flight is what Pause is for, and doing it here would
        // make one click on a queue interrupt several transfers that were
        // going perfectly well.
        if enabled {
            pump(Arc::clone(&self.inner)).await;
        }
        self.list_queues().await
    }

    /// Removes a queue, keeping its downloads.
    async fn remove_queue(&self, id: i64) -> Response {
        let Some(store) = self.inner.store.clone() else {
            return Response::Rejected { reason: NO_STORE.into() };
        };
        match store.remove_queue(id) {
            Ok(false) => return Response::Rejected { reason: "there is no such queue".into() },
            Err(error) => return Response::Rejected { reason: error.to_string() },
            Ok(true) => {}
        }

        // The row's own `queue_id` is cleared by the foreign key; the live
        // jobs are not, and one left pointing at a queue that has gone would
        // be read as unqueued by the scheduler anyway. Cleared here so that
        // the window stops showing a name that no longer exists.
        {
            let mut jobs = self.inner.jobs.lock().await;
            for job in jobs.values_mut() {
                if job.queue == Some(id) {
                    job.queue = None;
                    job.status.queue = None;
                }
            }
        }
        self.reload_queues().await;
        pump(Arc::clone(&self.inner)).await;
        self.list_queues().await
    }

    /// Moves a download into a queue, or out of one.
    async fn assign_queue(&self, id: JobId, queue: Option<i64>) -> Response {
        if let Some(wanted) = queue {
            let queues = self.inner.queues.lock().await;
            if crate::queues::find(&queues, wanted).is_none() {
                return Response::Rejected { reason: "that queue no longer exists".into() };
            }
        }

        let moved = {
            let mut jobs = self.inner.jobs.lock().await;
            match jobs.get_mut(&id) {
                Some(job) => {
                    job.queue = queue;
                    job.status.queue = queue;
                    true
                }
                None => false,
            }
        };
        if !moved {
            return Response::Rejected { reason: "there is no such download".into() };
        }

        if let (Some(store), Some(row)) =
            (self.inner.store.clone(), self.inner.rows.lock().await.get(&id).copied())
            && let Err(error) = store.assign_to_queue(row, queue)
        {
            tracing::warn!(%error, "could not record the queue change");
        }

        // Taking a download out of a queue, or into one with room, may mean
        // it can start now.
        pump(Arc::clone(&self.inner)).await;
        self.list_queues().await
    }

    /// Re-reads the queues from the store into the copy the scheduler uses.
    async fn reload_queues(&self) {
        if let Some(store) = &self.inner.store {
            *self.inner.queues.lock().await = read_queues(store.as_ref());
        }
    }

    /// The most of an archive's index that will be fetched.
    ///
    /// A directory is about fifty bytes an entry, so this is tens of
    /// thousands of files -- past the point where a list is useful, and
    /// well short of a number a hostile archive could use to make this
    /// hold a lot of memory.
    const MOST_INDEX_BYTES: usize = 2 << 20;

    /// Lists what is inside an archive, reading only its index.
    ///
    /// Two reads at most: the tail, which holds the record saying where
    /// the index is, and then the index itself -- unless the tail already
    /// contained it, which it does for any archive of a few thousand
    /// files. A four-gigabyte archive costs the same as a small one.
    /// Collects the links on a page.
    ///
    /// The refusals here are the same shape as `peek_archive`'s and for a
    /// stronger version of the same reason: this one makes the engine
    /// fetch, in sequence, a series of addresses it worked out for itself.
    /// Writes everything still to download out as a text list.
    async fn export_list(&self) -> Response {
        let Some(store) = self.inner.store.clone() else {
            return Response::Error { message: "there is no store to read".into() };
        };
        // The store rather than the live job map, because mirrors are kept
        // on the row: a download restored after a restart has them there
        // and nowhere else.
        let rows = match tokio::task::spawn_blocking(move || store.unfinished_jobs()).await {
            Ok(Ok(rows)) => rows,
            Ok(Err(error)) => return Response::Error { message: error.to_string() },
            Err(error) => return Response::Error { message: error.to_string() },
        };

        let items: Vec<crate::list::Wanted> = rows
            .into_iter()
            .map(|row| crate::list::Wanted {
                url: row.url,
                mirrors: row.mirrors,
                // The name it is actually being saved as, not the one the
                // address suggests: somebody who renamed a download meant
                // it, and an export that dropped the rename would come
                // back as a different file.
                filename: Some(row.filename),
                referrer: row.referrer,
            })
            .collect();

        Response::Listed { text: crate::list::write(&items), items: items.len() }
    }

    /// Reads a list of downloads and queues what is in it.
    async fn import_list(
        &self,
        client: &ClientInfo,
        name: &str,
        text: &str,
        queue: Option<i64>,
    ) -> Response {
        let wanted = crate::list::read(name, text);
        if wanted.is_empty() {
            return Response::Rejected {
                reason: "there were no web addresses in that file".into(),
            };
        }

        let mut added = 0;
        let mut skipped = 0;
        for item in wanted {
            let request = idm_ipc::DownloadRequest {
                url: item.url,
                mirrors: item.mirrors,
                filename: item.filename,
                referrer: item.referrer,
                queue,
                ..Default::default()
            };
            // Through `enqueue`, not around it: a list is a file from
            // somewhere else, and every rule a typed address goes through
            // applies to one that arrived in a text file.
            match self.enqueue(client, request).await {
                Response::Accepted { .. } => added += 1,
                _ => skipped += 1,
            }
        }

        Response::Imported { added, skipped }
    }

    async fn grab_links(&self, url: String, follow: idm_ipc::GrabDepth) -> Response {
        // The same vetting a download gets. Narrowed further inside the
        // grabber, which takes http and https only -- a magnet names a
        // swarm and has no links on it.
        let request = idm_ipc::DownloadRequest { url: url.clone(), ..Default::default() };
        if let Err(reason) = request.validate_with(true) {
            return Response::Rejected { reason };
        }

        let settings = self.inner.settings.lock().await.clone();
        let http = idm_net::client::HttpConfig {
            proxy: crate::proxy::mode_for(&settings, &url).await,
            ..Default::default()
        };

        let depth = match follow {
            idm_ipc::GrabDepth::ThisPage => crate::grab::Depth::ThisPage,
            idm_ipc::GrabDepth::AndWhatItLinksTo => crate::grab::Depth::AndWhatItLinksTo,
        };

        match crate::grab::grab(&http, &url, depth).await {
            Ok(grab) => Response::Grabbed {
                links: grab
                    .links
                    .into_iter()
                    .map(|link| idm_ipc::GrabbedLinkView {
                        category: crate::category::category_for(
                            link.url.rsplit('/').next().unwrap_or_default(),
                        )
                        .map(str::to_owned),
                        url: link.url,
                        text: link.text,
                        from: link.from,
                    })
                    .collect(),
                pages: grab.pages.len(),
                note: grab.note,
            },
            Err(error) => Response::Rejected { reason: error.to_string() },
        }
    }

    async fn peek_archive(&self, url: String) -> Response {
        // A fetch of this program's choosing, to an address somebody else
        // would be naming. That is the shape of request a page, or the
        // network, should not be able to make this machine send.

        // The same vetting a download gets, because this is the same kind
        // of act: an address from a client, fetched by this process.
        let request = idm_ipc::DownloadRequest { url: url.clone(), ..Default::default() };
        if let Err(reason) = request.validate_with(true) {
            return Response::Rejected { reason };
        }

        let settings = self.inner.settings.lock().await.clone();
        let http = idm_net::client::HttpConfig {
            proxy: crate::proxy::mode_for(&settings, &url).await,
            ..Default::default()
        };

        let tail = match idm_net::range::read_suffix(
            &http,
            &url,
            crate::archive::TAIL_BYTES,
            crate::archive::TAIL_BYTES * 2,
        )
        .await
        {
            Ok(window) => window,
            // Hitting the cap on a *tail* read means the server sent the
            // file from the beginning instead of its end, which is the
            // same refusal as below said from a different angle. The byte
            // count is accurate and tells nobody anything.
            Err(idm_net::range::RangeError::TooLarge { .. }) => {
                return Response::Rejected { reason: WHOLE_FILE_ONLY.into() };
            }
            Err(error) => return Response::Rejected { reason: error.to_string() },
        };

        if !tail.is_ranged() {
            // The server sent the whole file from the start. For a small
            // archive that is still usable -- the index is in there -- but
            // for a large one these are the opening bytes and the index is
            // not among them.
            let Some(total) = tail.total else {
                return Response::Rejected {
                    reason: "that server will not send part of a file, so this would mean \
                             downloading all of it"
                        .into(),
                };
            };
            if total as usize > tail.bytes.len() {
                return Response::Rejected {
                    reason: "that server will not send part of a file, so this would mean \
                             downloading all of it"
                        .into(),
                };
            }
        }

        let tail_at = tail.offset.unwrap_or(0);
        let index = match crate::archive::find_index(&tail.bytes, tail_at) {
            Ok(index) => index,
            Err(error) => return Response::Rejected { reason: error.to_string() },
        };

        if index.size as usize > Self::MOST_INDEX_BYTES {
            return Response::Rejected {
                reason: "that archive lists too many files to preview".into(),
            };
        }

        // Already in hand when the index sits inside the tail, which it
        // does for anything short of a few thousand files. Saves a round
        // trip on the common case rather than always asking twice.
        let directory = match index.offset.checked_sub(tail_at) {
            Some(within)
                if (within as usize).saturating_add(index.size as usize) <= tail.bytes.len() =>
            {
                tail.bytes[within as usize..(within + index.size) as usize].to_vec()
            }
            _ => match idm_net::range::read_at(
                &http,
                &url,
                index.offset,
                index.size as usize,
                Self::MOST_INDEX_BYTES,
            )
            .await
            {
                Ok(window) if window.is_ranged() => window.bytes,
                Ok(_) => {
                    return Response::Rejected {
                        reason: "that server stopped honouring ranges partway".into(),
                    };
                }
                Err(error) => return Response::Rejected { reason: error.to_string() },
            },
        };

        match crate::archive::list(&directory, index.entries) {
            Ok(listing) => Response::Archive {
                total_uncompressed: listing.entries.iter().map(|entry| entry.uncompressed).sum(),
                entries: listing
                    .entries
                    .into_iter()
                    .map(|entry| idm_ipc::ArchiveEntryView {
                        name: entry.name,
                        compressed: entry.compressed,
                        uncompressed: entry.uncompressed,
                        is_directory: entry.is_directory,
                    })
                    .collect(),
                truncated: listing.truncated,
            },
            Err(error) => Response::Rejected { reason: error.to_string() },
        }
    }

    /// The rules, and what the schedule is doing right now.
    async fn list_bandwidth_rules(&self) -> Response {
        let rules = self.inner.rules.lock().await.clone();
        let now = crate::schedule::now();
        let from_a_rule = crate::schedule::in_force(now, &rules);
        let action = match from_a_rule {
            Some(action) => action,
            None => crate::schedule::Action::Limit(
                self.inner.settings.lock().await.speed_limit_bytes_per_sec,
            ),
        };

        Response::BandwidthRules {
            rules: rules
                .iter()
                .map(|rule| idm_ipc::BandwidthRuleView {
                    id: rule.id,
                    days: rule.days,
                    start_minute: rule.start_minute,
                    end_minute: rule.end_minute,
                    bytes_per_sec: match rule.action {
                        crate::schedule::Action::Limit(bytes) => bytes,
                        crate::schedule::Action::Stop => 0,
                    },
                    stop: rule.action == crate::schedule::Action::Stop,
                })
                .collect(),
            now: idm_ipc::ScheduleNow {
                bytes_per_sec: match action {
                    crate::schedule::Action::Limit(bytes) => bytes,
                    crate::schedule::Action::Stop => 0,
                },
                stopped: action == crate::schedule::Action::Stop,
                from_a_rule: from_a_rule.is_some(),
                changes_in: crate::schedule::next_change(now, &rules),
            },
        }
    }

    /// Adds a rule or replaces one, then puts the result into effect.
    #[allow(clippy::too_many_arguments)]
    async fn set_bandwidth_rule(
        &self,
        id: Option<i64>,
        days: u8,
        start_minute: u16,
        end_minute: u16,
        bytes_per_sec: u64,
        stop: bool,
    ) -> Response {
        let Some(store) = self.inner.store.clone() else {
            return Response::Rejected { reason: NO_SCHEDULE_STORE.into() };
        };
        if let Err(reason) = crate::schedule::vet(days, start_minute, end_minute) {
            return Response::Rejected { reason };
        }

        if let Err(error) =
            store.upsert_bandwidth_rule(id, days, start_minute, end_minute, bytes_per_sec, stop)
        {
            return Response::Rejected { reason: error.to_string() };
        }

        self.reload_rules().await;
        self.list_bandwidth_rules().await
    }

    async fn remove_bandwidth_rule(&self, id: i64) -> Response {
        let Some(store) = self.inner.store.clone() else {
            return Response::Rejected { reason: NO_SCHEDULE_STORE.into() };
        };
        if let Err(error) = store.remove_bandwidth_rule(id) {
            return Response::Rejected { reason: error.to_string() };
        }

        self.reload_rules().await;
        self.list_bandwidth_rules().await
    }

    /// Re-reads the rules and puts whatever they now say into effect.
    ///
    /// Wholesale rather than patching the copy in memory, as the queues do:
    /// the table is small, and a mirror edited in two places is a mirror
    /// that can disagree with the thing it mirrors.
    async fn reload_rules(&self) {
        if let Some(store) = &self.inner.store {
            *self.inner.rules.lock().await = read_rules(store.as_ref());
        }
        // At once, rather than at the next boundary. Somebody who has just
        // written a rule covering this minute expects it to bite now, and
        // the task may be asleep for another ten.
        if apply_schedule(&self.inner).await {
            enforce_stop(&self.inner).await;
            pump(Arc::clone(&self.inner)).await;
        }
    }

    /// What is saved, never the passwords themselves.
    fn list_credentials(&self) -> Response {
        match crate::credentials::views(self.inner.vault.as_ref()) {
            Ok(entries) => Response::Credentials { entries },
            Err(reason) => Response::Rejected { reason },
        }
    }

    /// Saves one, replacing whatever was saved for the same thing.
    fn set_credential(
        &self,
        scope: idm_ipc::CredentialScope,
        username: String,
        password: idm_ipc::SecretString,
    ) -> Response {
        match crate::credentials::set(
            self.inner.vault.as_ref(),
            &scope,
            &username,
            password.expose(),
        ) {
            // Deliberately not logged, not even the target: "a password was
            // saved for example.com" in a log file is a fact about this
            // person that nothing here needs to keep.
            Ok(()) => Response::Pong,
            Err(reason) => Response::Rejected { reason },
        }
    }

    fn remove_credential(&self, scope: idm_ipc::CredentialScope) -> Response {
        match crate::credentials::remove(self.inner.vault.as_ref(), &scope) {
            Ok(()) => Response::Pong,
            Err(reason) => Response::Rejected { reason },
        }
    }

    /// What the app can run, and whether it has each already.
    fn list_sidecars() -> Response {
        Response::Sidecars { tools: crate::sidecars::views() }
    }

    /// Fetches one, verifies it and installs it.
    ///
    /// Takes no engine lock for the duration -- this is a hundred megabytes
    /// over somebody's connection, and every download and the window's own
    /// ticker have to keep running underneath it.
    async fn fetch_sidecar(name: String) -> Response {
        match crate::sidecars::fetch(&name).await {
            Ok(done) => {
                tracing::info!(%done, "installed a sidecar");
                Response::Pong
            }
            Err(reason) => Response::Rejected { reason },
        }
    }

    /// What the settings say right now.
    ///
    /// Public because the web interface's supervisor has to notice the
    /// switch being flipped, and reading them through a `Request` would
    /// mean a client name for something that is not a client.
    pub async fn settings(&self) -> Settings {
        self.inner.settings.lock().await.clone()
    }

    async fn get_settings(&self) -> Response {
        Response::Settings(self.settings().await)
    }

    async fn set_settings(&self, settings: Settings) -> Response {
        // Only the desktop app and CLI may change settings. Letting the
        // browser side rewrite them would hand a web page the ability to turn
        // its own kill switch back on.
        // Vetted before anything is stored, while the person is still at the
        // screen: a folder that cannot be written to otherwise fails later,
        // mid-transfer, on a file that may have taken an hour.
        let mut settings = settings;
        match crate::folder::vet_download_dir(&settings.download_dir) {
            Ok(vetted) => settings.download_dir = vetted,
            Err(reason) => return Response::Rejected { reason },
        }

        // Same reasoning, and the same moment: a mistyped proxy otherwise
        // surfaces as every download failing to connect, which looks nothing
        // like the typo that caused it. Only checked when it is going to be
        // used, so a stale address left behind by switching back to System
        // is not an error.
        if settings.proxy == idm_ipc::ProxySetting::Manual {
            match crate::proxy::vet(&settings.proxy_address) {
                Ok(vetted) => settings.proxy_address = vetted,
                Err(reason) => return Response::Rejected { reason },
            }
        }

        // Same again, and the one check here that is a boundary rather than a
        // convenience: a hook is a program this process will launch, so the
        // rules about what it may be belong where they cannot be skipped.
        // Vetted against the folder that was just vetted above, so choosing
        // a hook and moving the download folder onto it in one save is
        // refused rather than half-applied.
        let downloads = crate::hook::landing_dirs(&settings.download_dir, &self.inner.output_dir);
        match crate::hook::vet(&settings.hook_program, &downloads) {
            Ok(vetted) => settings.hook_program = vetted,
            Err(reason) => return Response::Rejected { reason },
        }
        match crate::hook::vet_arguments(&settings.hook_arguments) {
            Ok(vetted) => settings.hook_arguments = vetted,
            Err(reason) => return Response::Rejected { reason },
        }

        // The hidden list is plain text from the window. Capped and trimmed so
        // a settings file cannot be made to grow without bound, and
        // de-duplicated so hiding the same browser twice is the same as once.
        let mut seen = std::collections::HashSet::new();
        settings.hidden_browsers = settings
            .hidden_browsers
            .iter()
            .map(|name| name.trim().chars().take(64).collect::<String>())
            .filter(|name| !name.is_empty() && seen.insert(name.to_ascii_lowercase()))
            .take(64)
            .collect();

        let mut current = self.inner.settings.lock().await;
        *current = settings.clone();
        drop(current);
        save_settings(&self.inner.settings_file, &settings);

        // Unconditionally, and through the schedule rather than straight
        // from the setting. This used to fire only when the number in the
        // settings had changed, which a schedule breaks both ways: a save
        // that changed something else would leave the schedule's limit
        // un-asserted, and a save that changed the number would write it
        // over a window that was supposed to be overriding it.
        //
        // Takes effect on the next read of every running download, not only
        // on the ones started afterwards — the usual reason for changing a
        // limit is that a download already running is in the way.
        apply_schedule(&self.inner).await;
        Response::Settings(settings)
    }

    /// Cancels an armed shutdown, if one is counting down.
    async fn cancel_post_action(&self) -> Response {
        match self.inner.armed.lock().await.take() {
            Some(armed) => {
                armed.canceller.cancel();
                Response::Pong
            }
            None => Response::Rejected { reason: "nothing is armed".into() },
        }
    }

    /// What the machine is about to do, if anything.
    ///
    /// Clears the entry once the countdown has run out or been cancelled,
    /// so a banner cannot outlive the thing it is warning about -- which
    /// would be worse than no banner, since the one thing it offers is a
    /// button to stop something already done.
    async fn armed_action(&self) -> Response {
        let mut armed = self.inner.armed.lock().await;
        let view = match armed.as_ref() {
            Some(current) if !current.canceller.is_cancelled() => {
                let left = current.ends_at.saturating_duration_since(std::time::Instant::now());
                (!left.is_zero()).then(|| idm_ipc::ArmedView {
                    action: current.action.describe().to_owned(),
                    seconds_left: left.as_secs(),
                })
            }
            _ => None,
        };
        if view.is_none() {
            *armed = None;
        }
        Response::Armed { armed: view }
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
    *inner.armed.lock().await = Some(Armed {
        canceller: armed.canceller(),
        action,
        ends_at: std::time::Instant::now() + armed.countdown(),
    });
    Some((armed, all_succeeded))
}

/// A post-download action in its countdown.
struct Armed {
    canceller: Canceller,
    action: PostAction,
    /// When it fires. Kept as a deadline and turned into "seconds left" on
    /// the way out, so the number the window shows comes from one clock.
    ends_at: std::time::Instant,
}

/// Runs an armed action, reporting the countdown so the user can stop it.
async fn run_post_action(
    armed: ArmedAction,
    all_succeeded: bool,
    store: Option<Arc<idm_store::Store>>,
) {
    let action = armed.action();
    eprintln!(
        "All downloads finished. Will {} in {}s unless cancelled.",
        action.describe(),
        armed.countdown().as_secs()
    );

    // Before the countdown, not after: three of the four actions take the
    // machine away, and `armed.run` performs them itself -- by the time it
    // returns `Performed(Shutdown)`, Windows is already closing the process.
    // There is nothing left to write in any case, since an action only arms
    // once every download has landed, so a cancelled countdown has cost
    // nothing more than a checkpoint that was due anyway.
    if let Some(store) = &store
        && let Err(error) = store.checkpoint()
    {
        tracing::warn!(%error, "could not fold the database log back before the post-download action");
    }

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

/// Said when a queue change arrives and there is nowhere to keep it.
///
/// The engine runs without the database on purpose -- a database that will
/// not open should cost a re-climbed ramp, never a download -- but a queue is
/// nothing if it does not survive a restart, so this is a refusal rather than
/// a queue that quietly forgets itself.
const NO_STORE: &str = "queues need the database, which could not be opened";

/// The same, for the schedule, and for the same reason: a rule that did not
/// survive a restart would be worse than no rule, because somebody would
/// have set it and believed it.
/// Said when a server will not serve part of a file.
///
/// Looking inside an archive that way would mean fetching all of it, which
/// is the opposite of what the feature is for.
const WHOLE_FILE_ONLY: &str =
    "that server will not send part of a file, so this would mean downloading all of it";

const NO_SCHEDULE_STORE: &str = "a download schedule needs the database, which could not be opened";

/// The bandwidth rules as the store holds them, in the form the schedule
/// module wants.
///
/// A failed read means no rules, which is the safe direction here in the
/// opposite sense to queues: no rules means downloads run unthrottled,
/// where an invented rule could stop them for hours with nothing on screen
/// explaining why.
fn read_rules(store: &idm_store::Store) -> Vec<crate::schedule::Rule> {
    match store.bandwidth_rules() {
        Ok(rules) => rules
            .into_iter()
            .map(|rule| crate::schedule::Rule {
                id: rule.id,
                days: rule.days,
                start_minute: rule.start_minute,
                end_minute: rule.end_minute,
                action: if rule.stop {
                    crate::schedule::Action::Stop
                } else {
                    crate::schedule::Action::Limit(rule.bytes_per_sec)
                },
            })
            .collect(),
        Err(error) => {
            tracing::warn!(%error, "could not read the bandwidth schedule");
            Vec::new()
        }
    }
}

/// The queues as the store holds them, in the form the scheduler wants.
fn read_queues(store: &idm_store::Store) -> Vec<crate::queues::Queue> {
    match store.queues() {
        Ok(queues) => queues
            .into_iter()
            .map(|queue| crate::queues::Queue {
                id: queue.id,
                name: queue.name,
                max_concurrent: queue.max_concurrent,
                enabled: queue.enabled,
            })
            .collect(),
        Err(error) => {
            // Empty means every download starts freely, which is the old
            // behaviour and the safe direction: the alternative would be
            // holding downloads back on the strength of a failed read.
            tracing::warn!(%error, "could not read the queues");
            Vec::new()
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
        // One check, before the dispatch, from one exhaustive match in the
        // protocol crate. It used to be a check inside each handler, which
        // meant three handlers that never took a client -- `RefreshUrl`,
        // `ResumeFromHistory` and `GetSettings` among them -- were open to
        // the browser and to the network by omission rather than by
        // decision. See `Request::local_only`.
        if let Some(what) = request.local_only()
            && !client.is_trusted()
        {
            return Response::Rejected { reason: format!("{} cannot {what}", client.described()) };
        }

        match request {
            Request::Ping => Response::Pong,
            Request::Enqueue(req) => self.enqueue_batch(client, req).await,
            Request::Status { id } => self.status(id).await,
            Request::Cancel { id } => self.cancel(id).await,
            Request::Resume { id } => self.resume(id).await,
            Request::ResumeFromHistory { row_id, url } => self.resume_history(row_id, url).await,
            Request::Pause { id } => self.pause(id).await,
            Request::RefreshUrl { id, url, referrer, cookies } => {
                self.refresh_url(id, url, referrer, cookies).await
            }
            Request::RequestPairing { label } => self.request_pairing(client, label).await,
            Request::ListPairings => self.list_pairings().await,
            Request::ApprovePairing { request_id } => self.approve_pairing(request_id).await,
            Request::DenyPairing { request_id } => self.deny_pairing(request_id).await,
            Request::RevokePairing { client_id } => self.revoke_pairing(client_id).await,
            Request::SetBrowserEnabled { client_id, enabled } => {
                self.set_browser_enabled(client_id, enabled).await
            }
            Request::ListQueues => self.list_queues().await,
            Request::SetQueue { name, max_concurrent } => {
                self.set_queue(name, max_concurrent).await
            }
            Request::EnableQueue { id, enabled } => self.enable_queue(id, enabled).await,
            Request::RemoveQueue { id } => self.remove_queue(id).await,
            Request::AssignQueue { id, queue } => self.assign_queue(id, queue).await,
            Request::GetSettings => self.get_settings().await,
            Request::SetSettings(settings) => self.set_settings(settings).await,
            Request::CancelPostAction => self.cancel_post_action().await,
            Request::ArmedAction => self.armed_action().await,
            Request::PeekArchive { url } => self.peek_archive(url).await,
            Request::GrabLinks { url, follow } => self.grab_links(url, follow).await,
            Request::ExportList => self.export_list().await,
            Request::ImportList { name, text, queue } => {
                self.import_list(client, &name, &text, queue).await
            }
            Request::ListBandwidthRules => self.list_bandwidth_rules().await,
            Request::SetBandwidthRule {
                id,
                days,
                start_minute,
                end_minute,
                bytes_per_sec,
                stop,
            } => {
                self.set_bandwidth_rule(id, days, start_minute, end_minute, bytes_per_sec, stop)
                    .await
            }
            Request::RemoveBandwidthRule { id } => self.remove_bandwidth_rule(id).await,
            Request::ListCredentials => self.list_credentials(),
            Request::SetCredential { scope, username, password } => {
                self.set_credential(scope, username, password)
            }
            Request::RemoveCredential { scope } => self.remove_credential(scope),
            Request::ListSidecars => Self::list_sidecars(),
            Request::FetchSidecar { name } => Self::fetch_sidecar(name).await,
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

    eprintln!("DivinePulse serving");
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

    fn remote() -> ClientInfo {
        ClientInfo { name: idm_ipc::REMOTE_CLIENT.to_owned(), client_id: None }
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
        engine.approve_pairing(request_id).await;
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

    #[test]
    fn a_magnet_link_is_named_after_itself_and_not_after_its_address() {
        // Taking the leaf of the address, which is what every other download
        // does, gives `download.bin` for every magnet there is: a magnet has
        // no path to take a leaf from.
        let (_dir, engine) = engine();
        let link = "magnet:?xt=urn:btih:c9e15763f722f23e98a29decdfae341b98d53056\
                    &dn=Ubuntu+24.04.1+Desktop";
        assert_eq!(engine.vet(&request(link), 0, true).unwrap(), "Ubuntu 24.04.1 Desktop");
    }

    #[test]
    fn a_torrent_is_never_too_small_to_be_worth_taking_over() {
        // The takeover threshold measures the thing being fetched. A
        // `.torrent` is thirty kilobytes of bencode standing for however
        // large the swarm's files are, so measuring it refuses every torrent
        // a browser ever hands over -- the same trap a stream manifest sets.
        let (_dir, engine) = engine();
        let threshold = 10 << 20;

        for address in [
            "magnet:?xt=urn:btih:c9e15763f722f23e98a29decdfae341b98d53056",
            "https://releases.example.org/ubuntu.iso.torrent",
        ] {
            let mut small = request(address);
            small.size_hint = Some(30 << 10);
            assert!(
                engine.vet(&small, threshold, true).is_ok(),
                "{address} should not be refused for being small"
            );
        }

        // And the threshold still does its job on an ordinary download.
        let mut small = request("https://example.invalid/tiny.bin");
        small.size_hint = Some(30 << 10);
        assert!(engine.vet(&small, threshold, true).is_err());
    }

    #[test]
    fn a_torrent_file_on_this_machine_is_only_for_the_app_to_name() {
        // `vet` is where the browser/app distinction is enforced for every
        // other local-ish address, and a path to a file is the one a page
        // must never be able to choose.
        let (_dir, engine) = engine();
        let path = r"D:\downloads\ubuntu.torrent";
        assert!(engine.vet(&request(path), 0, true).is_ok());
        assert!(engine.vet(&request(path), 0, false).is_err());
    }

    #[test]
    fn a_category_folder_is_not_nested_inside_itself_on_a_resume() {
        // A torrent is placed from what it contains, which is known only
        // after it starts. A job resumed from its stored row therefore
        // arrives with the folder it was already given -- and deciding again
        // without this would produce `Downloads/Video/Video`, then
        // `Downloads/Video/Video/Video`.
        assert_eq!(base_of(Path::new(r"D:\Downloads\Video")), PathBuf::from(r"D:\Downloads"));
        assert_eq!(base_of(Path::new(r"D:\Downloads")), PathBuf::from(r"D:\Downloads"));

        // Every folder the sorter can produce has to be recognised, or the
        // nesting comes back for whichever one was missed.
        for category in crate::category::all_categories() {
            let dir = PathBuf::from(r"D:\Downloads").join(category);
            assert_eq!(base_of(&dir), PathBuf::from(r"D:\Downloads"), "{category}");
        }

        // A folder of the user's own that merely happens to sit at the end
        // of the path is not a category and must not be stripped.
        assert_eq!(
            base_of(Path::new(r"D:\Downloads\Linux ISOs")),
            PathBuf::from(r"D:\Downloads\Linux ISOs")
        );
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

        // Approving itself would defeat the whole mechanism. Through
        // `handle`, not the method: the refusal now sits in front of the
        // dispatch, so calling the method is not the path a client takes.
        let approving = Request::ApprovePairing { request_id: request_id.clone() };
        let self_approval = engine.handle(&client, approving).await;
        assert!(matches!(self_approval, Response::Rejected { .. }), "{self_approval:?}");
        // And it must not get to see what else is connected.
        assert!(matches!(
            engine.handle(&client, Request::ListPairings).await,
            Response::Rejected { .. }
        ));

        // The app can.
        assert!(matches!(
            engine.handle(&desktop(), Request::ApprovePairing { request_id }).await,
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

        engine.revoke_pairing("ext-1".into()).await;
        match engine.enqueue(&client, request("https://example.invalid/a.bin")).await {
            Response::Rejected { reason } => assert!(reason.contains("not paired"), "{reason}"),
            other => panic!("a revoked client should be refused, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_browser_switched_off_is_refused_but_stays_paired() {
        // Off is not the same as revoked. Revoking is a decision about trust
        // and has to be re-approved; "not today" must not cost that.
        let (_dir, engine) = engine();
        let client = browser();
        pair(&engine, &client).await;

        assert!(matches!(engine.set_browser_enabled("ext-1".into(), false).await, Response::Pong));
        match engine.enqueue(&client, request("https://example.invalid/a.bin")).await {
            Response::Rejected { reason } => assert!(reason.contains("switched off"), "{reason}"),
            other => panic!("a switched-off browser should be refused, got {other:?}"),
        }

        // Still approved: the pairing list has it, and switching it back on
        // needs nothing from the extension.
        let Response::Pairings { paired, .. } =
            engine.handle(&desktop(), Request::ListPairings).await
        else {
            panic!("expected the pairings");
        };
        assert_eq!(paired.len(), 1, "it must still be paired");
        assert!(!paired[0].enabled);

        engine.set_browser_enabled("ext-1".into(), true).await;
        assert!(matches!(
            engine.enqueue(&client, request("https://example.invalid/a.bin")).await,
            Response::Accepted { .. }
        ));
    }

    #[tokio::test]
    async fn a_greeting_from_a_switched_off_browser_says_so() {
        // The extension's greeting doubles as its status check. If a
        // switched-off browser were told "approved" it would report
        // connected while every download it handed over was declined.
        let (_dir, engine) = engine();
        let client = browser();
        pair(&engine, &client).await;
        engine.set_browser_enabled("ext-1".into(), false).await;

        let reply = engine.request_pairing(&client, "Chrome on this PC".into()).await;
        match reply {
            Response::Rejected { reason } => assert!(reason.contains("switched off"), "{reason}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_browser_cannot_switch_itself_back_on() {
        let (_dir, engine) = engine();
        let client = browser();
        pair(&engine, &client).await;
        engine.set_browser_enabled("ext-1".into(), false).await;

        let reply = engine
            .handle(
                &client,
                Request::SetBrowserEnabled { client_id: "ext-1".into(), enabled: true },
            )
            .await;
        assert!(matches!(reply, Response::Rejected { .. }), "{reply:?}");
        assert!(
            matches!(
                engine.enqueue(&client, request("https://example.invalid/a.bin")).await,
                Response::Rejected { .. }
            ),
            "and it must still be off"
        );
    }

    #[tokio::test]
    async fn switching_off_a_browser_that_is_not_paired_is_refused() {
        let (_dir, engine) = engine();
        let reply = engine.set_browser_enabled("nobody".into(), false).await;
        assert!(matches!(reply, Response::Rejected { .. }), "{reply:?}");
    }

    #[tokio::test]
    async fn the_switch_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let make = || {
            Engine::with_settings_file(
                dir.path().join("downloads"),
                8,
                dir.path().join("settings.json"),
            )
        };

        let first = make();
        pair(&first, &browser()).await;
        first.set_browser_enabled("ext-1".into(), false).await;
        drop(first);

        let second = make();
        assert!(
            matches!(
                second.enqueue(&browser(), request("https://example.invalid/a.bin")).await,
                Response::Rejected { .. }
            ),
            "a browser switched off must still be off after a restart"
        );
    }

    #[tokio::test]
    async fn a_pairing_written_before_the_switch_existed_still_works() {
        // pairings.json from an earlier version has no `enabled`. Treating
        // that as off would silently stop every already-paired browser.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pairings.json"),
            r#"{"paired":[{"client_id":"ext-1","label":"Chrome on this PC","paired_at_unix":1}]}"#,
        )
        .unwrap();
        let engine = Engine::with_settings_file(
            dir.path().join("downloads"),
            8,
            dir.path().join("settings.json"),
        );
        assert!(matches!(
            engine.enqueue(&browser(), request("https://example.invalid/a.bin")).await,
            Response::Accepted { .. }
        ));
    }

    #[tokio::test]
    async fn hidden_browsers_are_kept_trimmed_and_capped() {
        let (_dir, engine) = engine();
        let mut settings = match engine.handle(&desktop(), Request::GetSettings).await {
            Response::Settings(settings) => settings,
            other => panic!("expected settings, got {other:?}"),
        };
        settings.hidden_browsers = vec![
            " Firefox ".into(),
            "firefox".into(), // the same one again, differently cased
            "".into(),
            "x".repeat(500),
        ];
        let Response::Settings(saved) =
            engine.handle(&desktop(), Request::SetSettings(settings)).await
        else {
            panic!("expected settings back");
        };
        assert_eq!(saved.hidden_browsers[0], "Firefox");
        assert_eq!(saved.hidden_browsers.len(), 2, "{:?}", saved.hidden_browsers);
        assert_eq!(saved.hidden_browsers[1].len(), 64, "a name is cut to a sane length");

        let mut many = saved;
        many.hidden_browsers = (0..200).map(|n| format!("browser-{n}")).collect();
        let Response::Settings(capped) =
            engine.handle(&desktop(), Request::SetSettings(many)).await
        else {
            panic!("expected settings back");
        };
        assert_eq!(capped.hidden_browsers.len(), 64);
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
        assert!(matches!(engine.set_settings(off).await, Response::Settings(_)));

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
        engine.set_settings(off).await;

        let on = Settings { browser_integration: true, ..Settings::default() };
        assert!(matches!(
            engine.handle(&browser(), Request::SetSettings(on)).await,
            Response::Rejected { .. }
        ));

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
            engine.set_settings(changed).await;
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
        engine.set_settings(shutdown).await;

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

    /// Reads the settings out, lets the caller change them, puts them back.
    async fn put_settings(engine: &Arc<Engine>, change: impl FnOnce(&mut Settings)) -> Response {
        let mut settings = match engine.handle(&desktop(), Request::GetSettings).await {
            Response::Settings(settings) => settings,
            other => panic!("expected settings, got {other:?}"),
        };
        change(&mut settings);
        engine.handle(&desktop(), Request::SetSettings(settings)).await
    }

    #[tokio::test]
    async fn a_proxy_address_carrying_a_password_is_refused() {
        // The unit tests cover the vetting; this covers it being *wired in*,
        // which is the half that silently stops working.
        let (_dir, engine) = engine();
        let reply = put_settings(&engine, |settings| {
            settings.proxy = idm_ipc::ProxySetting::Manual;
            settings.proxy_address = "http://someone:hunter2@127.0.0.1:3128".into();
        })
        .await;

        match reply {
            Response::Rejected { reason } => {
                assert!(reason.contains("kept in the clear"), "should say why: {reason}");
                assert!(
                    reason.contains("Saved passwords"),
                    "and where it should go instead: {reason}",
                );
            }
            other => panic!("a password in a proxy address should be refused, got {other:?}"),
        }

        // And nothing was kept from the refused change.
        match engine.handle(&desktop(), Request::GetSettings).await {
            Response::Settings(settings) => {
                assert_eq!(settings.proxy, idm_ipc::ProxySetting::System);
                assert_eq!(settings.proxy_address, "");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_usable_proxy_is_accepted_and_kept() {
        let (_dir, engine) = engine();
        let reply = put_settings(&engine, |settings| {
            settings.proxy = idm_ipc::ProxySetting::Manual;
            settings.proxy_address = "  socks5://127.0.0.1:1080  ".into();
        })
        .await;
        assert!(!matches!(reply, Response::Rejected { .. }), "should be accepted: {reply:?}");

        match engine.handle(&desktop(), Request::GetSettings).await {
            // Tidied on the way in, so what comes back is what will be used.
            Response::Settings(settings) => {
                assert_eq!(settings.proxy_address, "socks5://127.0.0.1:1080");
                assert_eq!(
                    crate::proxy::mode(&settings),
                    crate::proxy::ProxyMode::Manual("socks5://127.0.0.1:1080".to_owned())
                );
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_address_left_behind_does_not_block_going_back_to_the_system_proxy() {
        // Switching away from Manual must not be refused by whatever is
        // still sitting in the address box -- including something invalid
        // that was never accepted in the first place.
        let (_dir, engine) = engine();
        let reply = put_settings(&engine, |settings| {
            settings.proxy = idm_ipc::ProxySetting::System;
            settings.proxy_address = "nonsense".into();
        })
        .await;
        assert!(!matches!(reply, Response::Rejected { .. }), "should be accepted: {reply:?}");
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

    /// An engine with a database of its own, which queues need.
    ///
    /// Never the real one: `Engine::new` opens the live database in
    /// `%LOCALAPPDATA%`, and a test that wrote queues into it would leave
    /// them in the running app.
    /// Arms an action directly, the way `maybe_arm` does, without having to
    /// finish a real download first.
    async fn arm(
        engine: &Arc<Engine>,
        action: PostAction,
        countdown: std::time::Duration,
    ) -> ArmedAction {
        let armed = ArmedAction::new(action, countdown);
        *engine.inner.armed.lock().await = Some(Armed {
            canceller: armed.canceller(),
            action,
            ends_at: std::time::Instant::now() + countdown,
        });
        armed
    }

    #[tokio::test]
    async fn an_armed_shutdown_is_something_the_window_can_see() {
        // The whole bug. The engine armed these, counted them down, and
        // announced it with `eprintln!` -- which for a windowed app is
        // nowhere. Nothing reached the window, so nothing could be stopped.
        let (_dir, engine) = engine_with_queues();
        let _armed = arm(&engine, PostAction::Shutdown, std::time::Duration::from_secs(60)).await;

        let Response::Armed { armed } = engine.handle(&desktop(), Request::ArmedAction).await
        else {
            panic!("the engine should say what it is about to do");
        };
        let view = armed.expect("something is armed");
        assert_eq!(view.action, PostAction::Shutdown.describe());
        assert!(view.seconds_left > 0 && view.seconds_left <= 60, "{view:?}");
    }

    #[tokio::test]
    async fn nothing_armed_is_reported_as_nothing_rather_than_refused() {
        // The ordinary case, asked for on every tick. A rejection here would
        // put an error in the window's log twice a second forever.
        let (_dir, engine) = engine_with_queues();
        let Response::Armed { armed } = engine.handle(&desktop(), Request::ArmedAction).await
        else {
            panic!("expected an answer");
        };
        assert!(armed.is_none());
    }

    #[tokio::test]
    async fn cancelling_really_disarms_rather_than_only_hiding_it() {
        // A banner that goes away while the countdown carries on would be
        // worse than no banner at all, so this checks the action itself.
        let (_dir, engine) = engine_with_queues();
        let armed = arm(&engine, PostAction::Shutdown, std::time::Duration::from_secs(60)).await;

        engine.handle(&desktop(), Request::CancelPostAction).await;
        assert!(armed.is_cancelled(), "the action itself must be cancelled");

        let Response::Armed { armed: after } =
            engine.handle(&desktop(), Request::ArmedAction).await
        else {
            panic!("expected an answer");
        };
        assert!(after.is_none(), "and the window must stop showing it");
    }

    #[tokio::test]
    async fn a_countdown_that_has_run_out_stops_being_reported() {
        // Otherwise the banner outlives the thing it warns about, offering a
        // button to stop something that already happened.
        let (_dir, engine) = engine_with_queues();
        let _armed = arm(&engine, PostAction::Sleep, std::time::Duration::from_millis(1)).await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let Response::Armed { armed } = engine.handle(&desktop(), Request::ArmedAction).await
        else {
            panic!("expected an answer");
        };
        assert!(armed.is_none());
    }

    #[tokio::test]
    async fn the_network_may_not_ask_what_the_machine_is_about_to_do() {
        // It says this machine is about to be left unattended.
        assert!(Request::ArmedAction.local_only().is_some());
        let (_dir, engine) = engine_with_queues();
        let _armed = arm(&engine, PostAction::Shutdown, std::time::Duration::from_secs(60)).await;
        let reply = engine.handle(&ClientInfo::remote(), Request::ArmedAction).await;
        assert!(matches!(reply, Response::Rejected { .. }), "{reply:?}");
    }

    fn engine_with_queues() -> (tempfile::TempDir, Arc<Engine>) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(idm_store::Store::in_memory().unwrap());
        let engine = engine_with_store(dir.path(), store);
        (dir, engine)
    }

    /// Makes a queue through the same path the window uses.
    async fn make_queue(engine: &Arc<Engine>, name: &str, at_once: u32) -> i64 {
        let response = engine
            .handle(&desktop(), Request::SetQueue { name: name.into(), max_concurrent: at_once })
            .await;
        let Response::Queues { queues } = response else {
            panic!("expected the queues back, got {response:?}");
        };
        queues.iter().find(|queue| queue.name == name).expect("the new queue").id
    }

    async fn queued_request(url: &str, queue: i64) -> DownloadRequest {
        DownloadRequest { queue: Some(queue), ..request(url) }
    }

    #[tokio::test]
    async fn a_queue_holds_its_downloads_to_its_own_limit() {
        let (_dir, engine) = engine_with_queues();
        // Room for far more than the queue allows, so it is the queue doing
        // the holding back and not the global limit.
        set_limit(&engine, 8).await;
        let overnight = make_queue(&engine, "overnight", 2).await;

        for n in 0..5 {
            let request =
                queued_request(&format!("http://127.0.0.1:9/big-{n}.bin"), overnight).await;
            engine.enqueue(&desktop(), request).await;
        }

        assert_eq!(engine.outstanding().await, 2, "the queue allows two");
        assert_eq!(engine.waiting().await, 3);
    }

    #[tokio::test]
    async fn a_full_queue_does_not_hold_up_an_unqueued_download() {
        // The head-of-line case, end to end: the queued jobs are older, so
        // a scheduler that took simply the oldest waiting job would never
        // reach the unqueued one and the app would look frozen.
        let (_dir, engine) = engine_with_queues();
        set_limit(&engine, 8).await;
        let overnight = make_queue(&engine, "overnight", 1).await;

        for n in 0..3 {
            let request = queued_request(&format!("http://127.0.0.1:9/q-{n}.bin"), overnight).await;
            engine.enqueue(&desktop(), request).await;
        }
        engine.enqueue(&desktop(), request("http://127.0.0.1:9/free.bin")).await;

        assert_eq!(engine.outstanding().await, 2, "one from the queue, plus the unqueued one");
    }

    #[tokio::test]
    async fn switching_a_queue_off_starts_nothing_more() {
        let (_dir, engine) = engine_with_queues();
        set_limit(&engine, 8).await;
        let overnight = make_queue(&engine, "overnight", 4).await;
        engine.handle(&desktop(), Request::EnableQueue { id: overnight, enabled: false }).await;

        for n in 0..3 {
            let request = queued_request(&format!("http://127.0.0.1:9/q-{n}.bin"), overnight).await;
            engine.enqueue(&desktop(), request).await;
        }

        assert_eq!(engine.outstanding().await, 0, "a switched-off queue starts nothing");
        assert_eq!(engine.waiting().await, 3);

        engine.handle(&desktop(), Request::EnableQueue { id: overnight, enabled: true }).await;
        assert_eq!(engine.outstanding().await, 3, "switching it on releases them");
    }

    #[tokio::test]
    async fn raising_a_queue_limit_releases_what_was_waiting() {
        let (_dir, engine) = engine_with_queues();
        set_limit(&engine, 8).await;
        let overnight = make_queue(&engine, "overnight", 1).await;
        for n in 0..3 {
            let request = queued_request(&format!("http://127.0.0.1:9/q-{n}.bin"), overnight).await;
            engine.enqueue(&desktop(), request).await;
        }
        assert_eq!(engine.outstanding().await, 1);

        // Nothing finished here. Without a pump on the change, these would
        // sit waiting until some unrelated download happened to end.
        make_queue(&engine, "overnight", 3).await;
        assert_eq!(engine.outstanding().await, 3);
    }

    #[tokio::test]
    async fn taking_a_download_out_of_a_queue_lets_it_start() {
        let (_dir, engine) = engine_with_queues();
        set_limit(&engine, 8).await;
        let overnight = make_queue(&engine, "overnight", 1).await;

        let first = queued_request("http://127.0.0.1:9/a.bin", overnight).await;
        engine.enqueue(&desktop(), first).await;
        let second = queued_request("http://127.0.0.1:9/b.bin", overnight).await;
        let Response::Accepted { id } = engine.enqueue(&desktop(), second).await else {
            panic!("the second should be accepted");
        };
        assert_eq!(engine.waiting().await, 1);

        engine.handle(&desktop(), Request::AssignQueue { id, queue: None }).await;
        assert_eq!(engine.waiting().await, 0, "out of the queue, the global limit governs it");
    }

    #[tokio::test]
    async fn removing_a_queue_keeps_its_downloads_and_releases_them() {
        let (_dir, engine) = engine_with_queues();
        set_limit(&engine, 8).await;
        let overnight = make_queue(&engine, "overnight", 1).await;
        for n in 0..3 {
            let request = queued_request(&format!("http://127.0.0.1:9/q-{n}.bin"), overnight).await;
            engine.enqueue(&desktop(), request).await;
        }

        engine.handle(&desktop(), Request::RemoveQueue { id: overnight }).await;

        let Response::Status { jobs } = engine.status(None).await else { panic!("status") };
        assert_eq!(jobs.len(), 3, "tidying a queue away must not take its downloads with it");
        assert!(jobs.iter().all(|job| job.queue.is_none()), "they should be unqueued");
        assert_eq!(engine.outstanding().await, 3, "and free to start");
    }

    #[tokio::test]
    async fn a_download_says_which_queue_it_is_in() {
        let (_dir, engine) = engine_with_queues();
        let overnight = make_queue(&engine, "overnight", 2).await;
        let request = queued_request("http://127.0.0.1:9/a.bin", overnight).await;
        engine.enqueue(&desktop(), request).await;

        let Response::Status { jobs } = engine.status(None).await else { panic!("status") };
        assert_eq!(jobs[0].queue, Some(overnight));
        assert_eq!(
            jobs[0].queue_name.as_deref(),
            Some("overnight"),
            "the name is filled in on the way out, not stored on the job"
        );
    }

    #[tokio::test]
    async fn a_queue_that_does_not_exist_is_refused_rather_than_ignored() {
        // Ignoring it would start the download at once, which is the
        // opposite of what naming a queue asked for.
        let (_dir, engine) = engine_with_queues();
        let request = queued_request("http://127.0.0.1:9/a.bin", 404).await;
        assert!(matches!(engine.enqueue(&desktop(), request).await, Response::Rejected { .. }));
    }

    #[tokio::test]
    async fn a_nameless_queue_and_an_oversized_one_are_refused() {
        let (_dir, engine) = engine_with_queues();
        for bad in [
            Request::SetQueue { name: "   ".into(), max_concurrent: 2 },
            Request::SetQueue { name: "ok".into(), max_concurrent: 0 },
            Request::SetQueue {
                name: "ok".into(),
                max_concurrent: crate::queues::MOST_AT_ONCE + 1,
            },
        ] {
            assert!(
                matches!(engine.handle(&desktop(), bad).await, Response::Rejected { .. }),
                "should be refused"
            );
        }
    }

    #[tokio::test]
    async fn a_browser_cannot_manage_queues() {
        let (_dir, engine) = engine_with_queues();
        let browser = browser();
        pair(&engine, &browser).await;

        for request in [
            Request::ListQueues,
            Request::SetQueue { name: "theirs".into(), max_concurrent: 4 },
            Request::EnableQueue { id: 1, enabled: false },
            Request::RemoveQueue { id: 1 },
            Request::AssignQueue { id: JobId(1), queue: Some(1) },
        ] {
            assert!(
                matches!(engine.handle(&browser, request).await, Response::Rejected { .. }),
                "a browser must not decide what runs and when"
            );
        }
    }

    /// Every day of the week, which makes a rule apply whenever the test
    /// happens to run. A rule tied to a weekday would pass or fail by the
    /// day somebody checked out the repository.
    const EVERY_DAY: u8 = 0b0111_1111;

    /// A window covering right now, however wide it needs to be.
    ///
    /// Midnight to midnight: the clock inside the engine is the machine's
    /// own, and no paused tokio clock moves it, so the only rule guaranteed
    /// to be in force is one that covers the whole day.
    fn all_day(bytes_per_sec: u64, stop: bool) -> Request {
        Request::SetBandwidthRule {
            id: None,
            days: EVERY_DAY,
            start_minute: 0,
            end_minute: crate::schedule::END_OF_DAY,
            bytes_per_sec,
            stop,
        }
    }

    #[tokio::test]
    async fn a_rule_in_force_reaches_the_live_limiter() {
        // The join that matters: the rules are proved on their own in
        // `schedule`, but a rule that never reaches the limiter would slow
        // nothing down and every one of those tests would still pass.
        let (_dir, engine) = engine_with_queues();
        engine.inner.limiter.set_rate(0).await;

        let reply = engine.handle(&desktop(), all_day(500_000, false)).await;
        assert!(matches!(reply, Response::BandwidthRules { .. }), "{reply:?}");

        assert_eq!(
            engine.inner.limiter.rate(),
            500_000,
            "the rule should have taken effect without waiting for a boundary",
        );
    }

    #[tokio::test]
    async fn a_rule_wins_over_the_everyday_limit_in_both_directions() {
        // The decision the user made: inside a window the rule is the
        // answer, which is what makes "slow normally, full speed
        // overnight" expressible. Taking the lower of the two would make
        // the second half of this impossible.
        let (_dir, engine) = engine_with_queues();

        let settings = Settings { speed_limit_bytes_per_sec: 1_000_000, ..Settings::default() };
        engine.handle(&desktop(), Request::SetSettings(settings)).await;
        assert_eq!(engine.inner.limiter.rate(), 1_000_000, "no rules yet");

        // Tighter than the setting.
        let slow = engine.handle(&desktop(), all_day(100_000, false)).await;
        let Response::BandwidthRules { rules, .. } = slow else { panic!("expected the rules") };
        assert_eq!(engine.inner.limiter.rate(), 100_000);

        // And looser, which is the half a minimum would forbid. The same
        // rule changed rather than a second one added: two rules covering
        // the same window would both be in force, and the tighter would
        // rightly win -- which would be testing the overlap rule, not this.
        let open = engine
            .handle(
                &desktop(),
                Request::SetBandwidthRule {
                    id: Some(rules[0].id),
                    days: EVERY_DAY,
                    start_minute: 0,
                    end_minute: crate::schedule::END_OF_DAY,
                    bytes_per_sec: 0,
                    stop: false,
                },
            )
            .await;
        assert_eq!(engine.inner.limiter.rate(), 0, "a rule may lift the limit too: {open:?}");
    }

    #[tokio::test]
    async fn saving_unrelated_settings_does_not_wipe_out_a_rule() {
        // The hazard this commit fixes. `set_rate` used to fire only when
        // the number in the settings had changed, so a save that changed
        // something else left the schedule un-asserted, and one that
        // changed the number wrote straight over a window.
        let (_dir, engine) = engine_with_queues();
        engine.handle(&desktop(), all_day(250_000, false)).await;
        assert_eq!(engine.inner.limiter.rate(), 250_000);

        // Nothing to do with the speed limit.
        let settings = Settings { sort_into_folders: false, ..Settings::default() };
        engine.handle(&desktop(), Request::SetSettings(settings)).await;
        assert_eq!(engine.inner.limiter.rate(), 250_000, "the rule still decides");

        // And one that does change it.
        let settings = Settings { speed_limit_bytes_per_sec: 9_000_000, ..Settings::default() };
        engine.handle(&desktop(), Request::SetSettings(settings)).await;
        assert_eq!(
            engine.inner.limiter.rate(),
            250_000,
            "a rule in force is not overwritten by the everyday limit",
        );
    }

    #[tokio::test]
    async fn removing_the_last_rule_hands_the_limit_back_to_the_settings() {
        let (_dir, engine) = engine_with_queues();
        let settings = Settings { speed_limit_bytes_per_sec: 2_000_000, ..Settings::default() };
        engine.handle(&desktop(), Request::SetSettings(settings)).await;

        let reply = engine.handle(&desktop(), all_day(50_000, false)).await;
        let Response::BandwidthRules { rules, .. } = reply else { panic!("{reply:?}") };
        assert_eq!(engine.inner.limiter.rate(), 50_000);

        engine.handle(&desktop(), Request::RemoveBandwidthRule { id: rules[0].id }).await;
        assert_eq!(
            engine.inner.limiter.rate(),
            2_000_000,
            "with no rule left, the everyday limit applies again",
        );
    }

    #[tokio::test]
    async fn a_stop_window_holds_downloads_back_and_says_so() {
        let (_dir, engine) = engine_with_queues();
        let reply = engine.handle(&desktop(), all_day(0, true)).await;

        let Response::BandwidthRules { now, .. } = reply else { panic!("{reply:?}") };
        assert!(now.stopped, "the window should be in force: {now:?}");
        assert!(now.from_a_rule);

        // And nothing starts while it is.
        let request = DownloadRequest {
            url: "https://example.invalid/held.bin".into(),
            ..Default::default()
        };
        let queued = engine.handle(&desktop(), Request::Enqueue(request)).await;
        let Response::Accepted { id } = queued else { panic!("{queued:?}") };

        let jobs = engine.inner.jobs.lock().await;
        let job = jobs.get(&id).expect("the job should be registered");
        assert!(job.waiting, "it should be waiting rather than running: {:?}", job.status.state);
    }

    #[tokio::test]
    async fn the_schedule_says_what_it_is_doing_without_the_window_working_it_out() {
        // So the window does not need a second copy of the overlap rules
        // in JavaScript, free to disagree with the one that decides.
        let (_dir, engine) = engine_with_queues();
        let settings = Settings { speed_limit_bytes_per_sec: 7_000, ..Settings::default() };
        engine.handle(&desktop(), Request::SetSettings(settings)).await;

        let reply = engine.handle(&desktop(), Request::ListBandwidthRules).await;
        let Response::BandwidthRules { rules, now } = reply else { panic!("{reply:?}") };
        assert!(rules.is_empty());
        assert!(!now.from_a_rule, "nothing is scheduled");
        assert_eq!(now.bytes_per_sec, 7_000, "so the everyday limit is what is in force");
        assert_eq!(now.changes_in, None, "and nothing is going to change it");
    }

    #[tokio::test]
    async fn a_rule_that_could_never_apply_is_refused_in_words() {
        let (_dir, engine) = engine_with_queues();
        let request = Request::SetBandwidthRule {
            id: None,
            days: 0,
            start_minute: 9 * 60,
            end_minute: 17 * 60,
            bytes_per_sec: 0,
            stop: false,
        };
        match engine.handle(&desktop(), request).await {
            Response::Rejected { reason } => assert!(reason.contains("day"), "{reason}"),
            other => panic!("a rule with no days should be refused, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_browser_cannot_change_when_downloads_run() {
        // A schedule decides what runs and when, exactly as a queue does.
        let (_dir, engine) = engine_with_queues();
        let browser = browser();
        pair(&engine, &browser).await;

        for request in
            [Request::ListBandwidthRules, all_day(0, true), Request::RemoveBandwidthRule { id: 1 }]
        {
            match engine.handle(&browser, request).await {
                Response::Rejected { reason } => {
                    assert!(reason.contains("schedule"), "say what was refused: {reason}");
                }
                other => panic!("a browser must not touch the schedule, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn a_browser_cannot_see_or_change_saved_passwords() {
        // Listing is refused as firmly as writing. The list alone says
        // which sites this person has an account with, which is not
        // something a web page gets to learn even with no password on it.
        let (_dir, engine) = engine_with_queues();
        let browser = browser();
        pair(&engine, &browser).await;

        let scope = idm_ipc::CredentialScope::Site { url: "https://example.com".into() };
        for request in [
            Request::ListCredentials,
            Request::SetCredential {
                scope: scope.clone(),
                username: "me".into(),
                password: idm_ipc::SecretString::new("hunter2"),
            },
            Request::RemoveCredential { scope: scope.clone() },
        ] {
            match engine.handle(&browser, request).await {
                Response::Rejected { reason } => {
                    assert!(reason.contains("saved passwords"), "say what was refused: {reason}");
                }
                other => panic!("a browser must not touch saved passwords, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn the_app_may_ask_what_is_saved() {
        // Read-only, and only under this app's own prefix, so running it
        // cannot disturb anything in the machine's real vault.
        let (_dir, engine) = engine_with_queues();
        match engine.handle(&desktop(), Request::ListCredentials).await {
            Response::Credentials { entries } => {
                // Whatever is there, none of it carries a password: the
                // type has no field for one.
                let shown = format!("{entries:?}");
                assert!(!shown.contains("password"), "{shown}");
            }
            other => panic!("the app should get the list, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_password_for_something_that_cannot_ask_for_one_is_refused() {
        // Refused at the engine rather than stored against a scheme no
        // download will ever look up, which would show in the list as a
        // password that is set up and never work.
        let (_dir, engine) = engine_with_queues();
        let request = Request::SetCredential {
            scope: idm_ipc::CredentialScope::Site { url: "ftp://example.com".into() },
            username: "me".into(),
            password: idm_ipc::SecretString::new("hunter2"),
        };
        match engine.handle(&desktop(), request).await {
            Response::Rejected { reason } => assert!(reason.contains("http"), "{reason}"),
            other => panic!("that scheme should be refused, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_browser_cannot_install_a_program() {
        // The sharpest edge in the whole surface: `FetchSidecar` has the
        // engine download an executable and then run it. The name picks an
        // entry out of a compiled-in manifest rather than carrying an
        // address, so even an allowed caller cannot choose what is
        // fetched -- and a web page is not an allowed caller at all.
        let (_dir, engine) = engine_with_queues();
        let browser = browser();
        pair(&engine, &browser).await;

        for request in [Request::ListSidecars, Request::FetchSidecar { name: "ffmpeg".into() }] {
            match engine.handle(&browser, request).await {
                Response::Rejected { reason } => {
                    assert!(reason.contains("install"), "say what was refused: {reason}");
                }
                other => panic!("a browser must not install programs, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn the_app_is_told_what_it_can_fetch_without_fetching_anything() {
        let (_dir, engine) = engine_with_queues();
        match engine.handle(&desktop(), Request::ListSidecars).await {
            Response::Sidecars { tools } => {
                assert!(tools.iter().any(|tool| tool.name == "ffmpeg"), "{tools:?}");
                for tool in tools {
                    // The size is what the question is asked with, so a
                    // zero would make the asking dishonest.
                    assert!(tool.bytes > 0, "{} states no size", tool.name);
                    assert!(!tool.why.is_empty(), "{} does not say what it is for", tool.name);
                }
            }
            other => panic!("the app should get the list, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_app_cannot_fetch_something_that_is_not_in_the_manifest() {
        // The name is an index into a fixed list, not an instruction. A
        // client that invents one gets a refusal rather than a download.
        let (_dir, engine) = engine_with_queues();
        match engine.handle(&desktop(), Request::FetchSidecar { name: "curl".into() }).await {
            Response::Rejected { reason } => assert!(reason.contains("no sidecar"), "{reason}"),
            other => panic!("an unknown name should be refused, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_browser_cannot_name_a_queue_on_a_download() {
        let request = DownloadRequest {
            url: "https://example.invalid/a.bin".into(),
            queue: Some(1),
            ..Default::default()
        };
        assert!(request.validate().is_err(), "refused for a browser");
        assert!(request.validate_with(true).is_ok(), "allowed for the app");
    }

    #[tokio::test]
    async fn queues_without_a_database_are_refused_rather_than_forgotten() {
        // The engine runs without the store on purpose, but a queue that did
        // not survive a restart would be a worse lie than a refusal.
        let (_dir, engine) = engine();
        let response = engine
            .handle(&desktop(), Request::SetQueue { name: "x".into(), max_concurrent: 2 })
            .await;
        let Response::Rejected { reason } = response else { panic!("expected a refusal") };
        assert!(reason.contains("database"), "{reason}");
    }

    #[tokio::test]
    async fn a_queue_survives_a_restart_with_its_downloads_still_in_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(idm_store::Store::in_memory().unwrap());

        let first = engine_with_store(dir.path(), Arc::clone(&store));
        set_limit(&first, 8).await;
        let overnight = make_queue(&first, "overnight", 1).await;
        for n in 0..2 {
            let request = queued_request(&format!("http://127.0.0.1:9/q-{n}.bin"), overnight).await;
            first.enqueue(&desktop(), request).await;
        }

        // A second engine over the same rows, as a restart would be.
        let second = engine_with_store(dir.path(), store);
        set_limit(&second, 8).await;
        set_resume_on_start(&second, true).await;
        second.restore_interrupted().await;

        let Response::Queues { queues } = second.handle(&desktop(), Request::ListQueues).await
        else {
            panic!("expected the queues");
        };
        assert_eq!(queues.len(), 1, "the queue should still be there");
        assert_eq!(queues[0].max_concurrent, 1);
        assert_eq!(
            queues[0].running + queues[0].waiting,
            2,
            "both downloads should come back in it"
        );
        assert_eq!(second.outstanding().await, 1, "and still be held to its limit");
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
    async fn a_cancelled_download_can_be_resumed_from_its_history_row() {
        // Cancelling aborts the task that would otherwise drop this job's row
        // mapping on its way out. Left behind, the mapping made the row look
        // live for the rest of the process, and History's Resume refused to
        // start a download it believed was already going -- so a cancelled
        // download could not be resumed again until the app was restarted.
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

        assert!(matches!(engine.cancel(id).await, Response::Accepted { .. }));
        assert!(
            matches!(engine.resume_history(row, None).await, Response::Accepted { .. }),
            "a cancelled download must be resumable from the row it left in History",
        );
    }

    /// Makes a failed row in History, the way an expired link leaves one.
    fn failed_row(dir: &std::path::Path, store: &idm_store::Store, url: &str) -> i64 {
        let row = store
            .add_job(&idm_store::NewJob {
                url: url.into(),
                filename: "big.bin".into(),
                output_dir: dir.join("downloads").display().to_string(),
                ..Default::default()
            })
            .unwrap();
        store.set_job_state(row, idm_store::JobState::Failed, None).unwrap();
        row
    }

    #[tokio::test]
    async fn a_failed_download_can_be_picked_up_at_a_new_address() {
        // The case the whole refresh-address feature exists for: the bytes
        // on disk are fine and only the way back to the rest of them has
        // gone stale. Reachable from History, because a download that failed
        // is not in the job list any more -- and after a restart was never
        // in it.
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(idm_store::Store::open(&dir.path().join("state.sqlite3")).unwrap());
        let row = failed_row(dir.path(), &store, "http://127.0.0.1:9/expired.bin?sig=old");

        let engine = engine_with_store(dir.path(), Arc::clone(&store));
        let fresh = "http://127.0.0.1:9/expired.bin?sig=new".to_owned();
        assert!(matches!(
            engine.resume_history(row, Some(fresh.clone())).await,
            Response::Accepted { .. }
        ));

        let Response::Status { jobs } = engine.status(None).await else { panic!("status") };
        assert_eq!(jobs[0].url, fresh, "it should be going to the new address");
        assert_eq!(
            store.job(row).unwrap().unwrap().url,
            fresh,
            "and the row should keep it, so a second go does not use the dead one",
        );
    }

    #[tokio::test]
    async fn a_new_address_from_history_is_vetted_like_any_other() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(idm_store::Store::open(&dir.path().join("state.sqlite3")).unwrap());
        let row = failed_row(dir.path(), &store, "http://127.0.0.1:9/expired.bin");

        let engine = engine_with_store(dir.path(), Arc::clone(&store));
        for bad in ["", "javascript:alert(1)", "not a url at all"] {
            assert!(
                matches!(
                    engine.resume_history(row, Some(bad.into())).await,
                    Response::Rejected { .. }
                ),
                "{bad} should be refused",
            );
        }
        assert_eq!(
            store.job(row).unwrap().unwrap().url,
            "http://127.0.0.1:9/expired.bin",
            "a refused address must not have been written to the row",
        );
    }

    #[tokio::test]
    async fn picking_up_from_history_without_an_address_keeps_the_old_one() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(idm_store::Store::open(&dir.path().join("state.sqlite3")).unwrap());
        let row = failed_row(dir.path(), &store, "http://127.0.0.1:9/still-good.bin");

        let engine = engine_with_store(dir.path(), Arc::clone(&store));
        assert!(matches!(engine.resume_history(row, None).await, Response::Accepted { .. }));

        let Response::Status { jobs } = engine.status(None).await else { panic!("status") };
        assert_eq!(jobs[0].url, "http://127.0.0.1:9/still-good.bin");
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
    async fn everything_the_browser_is_refused_the_web_interface_is_refused_too() {
        // The point of the refactor, written as a test so it stays true.
        // Each of these is about the machine rather than about a download,
        // and the only thing that made them safe was that the browser was
        // the one client that was not the person sitting here. It is not
        // the only one any more.
        let (_dir, engine) = engine();
        let requests = || {
            vec![
                Request::ListQueues,
                Request::SetQueue { name: "x".into(), max_concurrent: 1 },
                Request::ListBandwidthRules,
                Request::ListCredentials,
                Request::ListSidecars,
                Request::ListPairings,
                Request::SetSettings(Settings::default()),
                Request::PeekArchive { url: "https://example.invalid/x.zip".into() },
                Request::GrabLinks {
                    url: "https://example.invalid/".into(),
                    follow: idm_ipc::GrabDepth::ThisPage,
                },
                Request::ExportList,
                Request::ImportList {
                    name: "x.txt".into(),
                    text: "https://example.invalid/x.bin".into(),
                    queue: None,
                },
                // The three that used to slip through, because their
                // handlers never took a client to check. The first two
                // carry a replacement address for a download that already
                // exists, which is the worst shape in the protocol: the
                // file keeps the name the user approved and the bytes come
                // from somewhere else.
                Request::RefreshUrl {
                    id: JobId(1),
                    url: "https://elsewhere.invalid/x.bin".into(),
                    referrer: None,
                    cookies: None,
                },
                Request::ResumeFromHistory {
                    row_id: 1,
                    url: Some("https://elsewhere.invalid/x.bin".into()),
                },
                // And this one hands out the download folder, the proxy
                // address and the program a finished download runs.
                Request::GetSettings,
                Request::CancelPostAction,
            ]
        };

        for (client, who) in [(browser(), "the browser"), (remote(), "the web interface")] {
            for request in requests() {
                let name = format!("{request:?}");
                let reply = engine.handle(&client, request).await;
                assert!(
                    matches!(reply, Response::Rejected { .. }),
                    "{who} was allowed {name}: {reply:?}",
                );
            }
        }
    }

    #[tokio::test]
    async fn the_browser_cannot_read_or_write_the_download_list() {
        // One says what this person is fetching and from where; the other
        // turns a block of text into any number of downloads.
        let (_dir, engine) = engine();
        assert!(matches!(
            engine.handle(&browser(), Request::ExportList).await,
            Response::Rejected { .. }
        ));
        let importing = Request::ImportList {
            name: "list.txt".into(),
            text: "https://example.invalid/x.bin".into(),
            queue: None,
        };
        assert!(matches!(engine.handle(&browser(), importing).await, Response::Rejected { .. }));
    }

    #[tokio::test]
    async fn a_list_of_addresses_is_queued() {
        let (_dir, engine) = engine();
        occupy_the_slot(&engine).await;

        let importing = Request::ImportList {
            name: "list.txt".into(),
            text: "# mine
https://example.invalid/a.bin
https://example.invalid/b.bin
"
            .into(),
            queue: None,
        };
        match engine.handle(&desktop(), importing).await {
            Response::Imported { added, skipped } => assert_eq!((added, skipped), (2, 0)),
            other => panic!("expected an import, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_file_with_no_addresses_in_it_is_refused() {
        // Rather than reported as importing nothing, which looks like a
        // success and leaves somebody wondering where their downloads went.
        let (_dir, engine) = engine();
        let importing = Request::ImportList {
            name: "notes.txt".into(),
            text: "shopping list
milk
"
            .into(),
            queue: None,
        };
        assert!(matches!(engine.handle(&desktop(), importing).await, Response::Rejected { .. }));
    }

    #[tokio::test]
    async fn what_is_exported_can_be_imported_again() {
        // The join the two halves are for. list.rs round-trips its own
        // format in its own tests; this is about the engine filling it from
        // real downloads and reading real downloads back out of it.
        // With a store, because mirrors live on the row rather than in
        // the live job map -- which is also why the export reads the store.
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(idm_store::Store::in_memory().unwrap());
        let engine = engine_with_store(dir.path(), store);
        occupy_the_slot(&engine).await;

        let request = DownloadRequest {
            url: "https://example.invalid/film.mp4".into(),
            mirrors: vec!["https://mirror.invalid/film.mp4".into()],
            size_hint: Some(10 << 20),
            ..Default::default()
        };
        assert!(matches!(engine.enqueue(&desktop(), request).await, Response::Accepted { .. }));

        let Response::Listed { text, items } = engine.handle(&desktop(), Request::ExportList).await
        else {
            panic!("expected a list");
        };
        assert!(items >= 1, "{text}");
        assert!(text.contains("https://mirror.invalid/film.mp4"), "{text}");

        let read = crate::list::read("downloads.txt", &text);
        let film = read
            .iter()
            .find(|item| item.url == "https://example.invalid/film.mp4")
            .expect("the download should be in the list");
        assert_eq!(film.mirrors, vec!["https://mirror.invalid/film.mp4"]);
        assert_eq!(film.filename.as_deref(), Some("film.mp4"));
    }

    /// Sets the post-download hook through the same path the window uses.
    async fn set_hook(engine: &Arc<Engine>, program: &str) -> Response {
        let mut settings = match engine.handle(&desktop(), Request::GetSettings).await {
            Response::Settings(settings) => settings,
            other => panic!("expected settings, got {other:?}"),
        };
        settings.hook_program = program.to_owned();
        engine.handle(&desktop(), Request::SetSettings(settings)).await
    }

    #[tokio::test]
    async fn a_hook_that_is_not_an_exe_is_refused_when_it_is_set() {
        // hook.rs has its own tests for every rule. This one is only about
        // the rules being reached from the request, because a validator
        // nothing calls would pass all of them.
        let (dir, engine) = engine();
        let batch = dir.path().join("after.bat");
        std::fs::write(&batch, b"echo hi").unwrap();

        let outcome = set_hook(&engine, &batch.display().to_string()).await;
        match outcome {
            Response::Rejected { reason } => assert!(reason.contains(".exe"), "{reason}"),
            other => panic!("a batch file must not be accepted as a hook: {other:?}"),
        }

        match engine.handle(&desktop(), Request::GetSettings).await {
            Response::Settings(settings) => assert_eq!(settings.hook_program, ""),
            other => panic!("expected settings, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_hook_inside_the_download_folder_is_refused_when_it_is_set() {
        let (dir, engine) = engine();
        // The engine's own default folder, which is a download folder even
        // when the setting naming one is empty.
        let program = dir.path().join("downloads");
        std::fs::create_dir_all(&program).unwrap();
        let program = program.join("after.exe");
        std::fs::write(&program, b"MZ").unwrap();

        match set_hook(&engine, &program.display().to_string()).await {
            Response::Rejected { reason } => {
                assert!(reason.contains("downloads are saved"), "{reason}")
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
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
            matches!(
                engine.refresh_url(id, fresh.into(), None, None).await,
                Response::Accepted { .. }
            ),
            "refresh"
        );

        let jobs = engine.inner.jobs.lock().await;
        let job = jobs.get(&id).expect("still there");
        assert_eq!(job.config.url, fresh, "the engine should fetch from the new address");
        assert_eq!(job.status.url, fresh, "and the window should show it");
        assert_eq!(job.status.filename, "f.bin", "the destination must not move");
    }

    #[test]
    fn only_a_page_with_no_length_is_worth_handing_to_yt_dlp() {
        use crate::failure::Kind;
        let page = "https://example.invalid/watch?v=x";
        assert!(worth_asking_yt_dlp(Kind::DownloadUnknownSize, page));

        // Everything yt-dlp would not improve. Asking it would turn a
        // quick, accurate refusal into a slow one.
        for kind in [
            Kind::HttpNotFound,
            Kind::ServerRangesUnsupported,
            Kind::Unclassified,
            Kind::StorageInsufficientSpace,
        ] {
            assert!(!worth_asking_yt_dlp(kind, page), "{kind:?}");
        }
    }

    #[test]
    fn schemes_the_engine_has_its_own_answers_for_are_left_alone() {
        use crate::failure::Kind;
        for url in [
            "ftp://example.invalid/f.bin",
            "sftp://example.invalid/f.bin",
            "magnet:?xt=urn:btih:abc",
            "file:///C:/x.bin",
        ] {
            assert!(!worth_asking_yt_dlp(Kind::DownloadUnknownSize, url), "{url}");
        }
    }

    #[tokio::test]
    async fn a_refreshed_link_can_bring_fresh_cookies_with_it() {
        // A signed link and the cookie that authorises it usually expire
        // together, so replacing only the address earns the same refusal a
        // moment later.
        let (_dir, engine) = engine();
        let mut first = request("https://old.invalid/f.bin");
        first.cookies = Some("session=stale".into());
        first.referrer = Some("https://old.invalid/page".into());
        let Response::Accepted { id } = engine.enqueue(&desktop(), first).await else {
            panic!("expected the job to be accepted");
        };
        engine.pause(id).await;

        engine
            .refresh_url(
                id,
                "https://new.invalid/f.bin?sig=abc".into(),
                Some("https://new.invalid/page".into()),
                Some("session=fresh".into()),
            )
            .await;

        let jobs = engine.inner.jobs.lock().await;
        let job = jobs.get(&id).expect("still there");
        assert_eq!(job.config.cookies.as_deref(), Some("session=fresh"));
        assert_eq!(job.config.referrer.as_deref(), Some("https://new.invalid/page"));
    }

    #[tokio::test]
    async fn a_refreshed_link_on_its_own_keeps_the_headers_it_had() {
        // Pasting an address and nothing else means "the headers are still
        // good". Clearing them would turn one expired thing into two, on
        // the download somebody is trying to rescue.
        let (_dir, engine) = engine();
        let mut first = request("https://old.invalid/f.bin");
        first.cookies = Some("session=good".into());
        first.referrer = Some("https://old.invalid/page".into());
        let Response::Accepted { id } = engine.enqueue(&desktop(), first).await else {
            panic!("expected the job to be accepted");
        };
        engine.pause(id).await;

        engine.refresh_url(id, "https://new.invalid/f.bin".into(), None, None).await;

        let jobs = engine.inner.jobs.lock().await;
        let job = jobs.get(&id).expect("still there");
        assert_eq!(job.config.cookies.as_deref(), Some("session=good"), "cookies kept");
        assert_eq!(job.config.referrer.as_deref(), Some("https://old.invalid/page"));
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
                matches!(
                    engine.refresh_url(id, hostile.into(), None, None).await,
                    Response::Rejected { .. }
                ),
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
