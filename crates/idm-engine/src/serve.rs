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
                armed: Mutex::new(None),
            }),
        })
    }

    /// Decides whether to accept a request, and under what filename.
    ///
    /// Returns a sanitised *leaf* name. The caller joins it to our output
    /// directory; nothing from the request contributes a path component.
    fn vet(&self, request: &DownloadRequest, min_takeover_bytes: u64) -> Result<String, String> {
        request.validate()?;

        if let Some(size) = request.size_hint {
            if size < min_takeover_bytes {
                return Err(format!(
                    "{size} bytes is below the {min_takeover_bytes} byte takeover threshold"
                ));
            }
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

        let filename = match self.vet(&request, settings.min_takeover_bytes) {
            Ok(name) => name,
            Err(reason) => return Response::Rejected { reason },
        };

        let id = JobId(self.inner.next_id.fetch_add(1, Ordering::SeqCst));
        let mut config = DownloadConfig::new(request.url.clone(), self.inner.output_dir.clone());
        config.filename = Some(filename.clone());
        config.conn.per_host_max = self.inner.max_connections;
        config.referrer = request.referrer.clone();
        config.cookies = request.cookies.clone();
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

        let inner = Arc::clone(&self.inner);
        let task = tokio::spawn(async move {
            set_state(&inner, id, JobState::Running).await;

            let progress_inner = Arc::clone(&inner);
            let outcome = download_with_progress(&config, move |progress: Progress| {
                record_progress(&progress_inner, id, &progress);
            })
            .await;

            match outcome {
                Ok(report) => {
                    finish(&inner, id, JobState::Completed, Some(report.total), None).await;
                }
                Err(error) => {
                    // The partial file and journal are deliberately left in
                    // place, so a retry resumes rather than restarts.
                    finish(&inner, id, JobState::Failed, None, Some(error.to_string())).await;
                }
            }
            forget_row(&inner, id).await;

            // Every finishing job asks whether the queue has now drained.
            // Doing it here rather than on a timer means the action fires as
            // soon as the last download lands.
            if let Some((armed, all_succeeded)) = maybe_arm(&inner).await {
                tokio::spawn(run_post_action(armed, all_succeeded));
            }
        });

        self.inner
            .jobs
            .lock()
            .await
            .insert(id, JobRecord { status, abort: Some(task.abort_handle()) });
        Response::Accepted { id }
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
    if let Ok(mut jobs) = inner.jobs.try_lock() {
        if let Some(record) = jobs.get_mut(&id) {
            record.status.downloaded = progress.durable;
            record.status.total = Some(progress.total);
            record.status.bytes_per_sec = progress.rate_bps;
            record.status.connections = progress.connections;
        }
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
        *current = settings.clone();
        save_settings(&self.inner.settings_file, &settings);
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
    // A missing or unreadable file means defaults, which are the safe ones:
    // browser integration on, no post-download action.
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
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
            if let Ok(name) = engine.vet(&req, 0) {
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
        assert_eq!(engine.vet(&req, 0).unwrap(), "_CON");
    }

    #[test]
    fn a_filename_is_derived_from_the_url_when_none_is_given() {
        let (_dir, engine) = engine();
        let name = engine.vet(&request("https://example.invalid/path/report.pdf?x=1"), 0).unwrap();
        assert_eq!(name, "report.pdf");
    }

    #[test]
    fn small_downloads_are_left_to_the_browser() {
        // Taking over a 100 KiB file costs a handover for no benefit.
        let (_dir, engine) = engine();
        let threshold = Settings::default().min_takeover_bytes;
        let mut req = request("https://example.invalid/small.bin");
        req.size_hint = Some(100 << 10);
        assert!(engine.vet(&req, threshold).is_err());

        req.size_hint = Some(50 << 20);
        assert!(engine.vet(&req, threshold).is_ok());
        // An unknown size is accepted; the probe will settle it.
        req.size_hint = None;
        assert!(engine.vet(&req, threshold).is_ok());
    }

    #[test]
    fn non_http_schemes_never_reach_the_engine() {
        let (_dir, engine) = engine();
        for hostile in ["file:///C:/secret", "data:text/plain,hi", "javascript:alert(1)"] {
            assert!(engine.vet(&request(hostile), 0).is_err(), "{hostile} should be refused");
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
}
