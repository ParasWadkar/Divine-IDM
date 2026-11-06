//! What the window is allowed to ask the engine, and how.

use std::sync::Arc;

use idm_ipc::server::Handler;
use idm_ipc::{ClientInfo, DownloadRequest, Request, Response};

pub struct AppState {
    pub engine: Arc<idm_engine::Engine>,
    /// Read-only companion handle for the history view.
    ///
    /// The engine owns its own handle and writes through it; this one only
    /// reads finished rows. Two handles on one SQLite file are what WAL is
    /// for, and the alternative — widening the IPC protocol with a history
    /// request — would put a query on the wire that only this process can
    /// usefully answer.
    store: Option<Arc<idm_store::Store>>,
    /// Identifies the window to the engine.
    ///
    /// Not a browser client, so the engine allows it to manage pairings and
    /// change settings; the browser kill switch does not apply to it, which
    /// is the whole point of a switch the app can still reach.
    client: ClientInfo,
    /// Files the shell opened with this app, waiting for the window to ask.
    ///
    /// Kept rather than only announced, because a double-click can launch the
    /// app from cold: the file is known before there is a window to tell, and
    /// an event sent into nothing is lost. The window collects these when it
    /// loads, and again whenever it is told there are more.
    opened: std::sync::Mutex<Vec<String>>,
}

impl AppState {
    pub fn new() -> Self {
        let output_dir = default_download_dir();
        Self {
            engine: idm_engine::Engine::new(output_dir, 16),
            store: idm_engine::open_store(),
            client: ClientInfo { name: "desktop".to_owned(), client_id: None },
            opened: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Records files the shell opened with this app.
    pub fn note_opened(&self, files: Vec<String>) {
        if let Ok(mut opened) = self.opened.lock() {
            opened.extend(files);
        }
    }

    /// Hands over, and forgets, everything recorded since the last call.
    ///
    /// Forgets so the same notice is not raised twice: it is a statement
    /// about something that just happened, not a standing fact.
    pub fn take_opened(&self) -> Vec<String> {
        self.opened.lock().map(|mut opened| std::mem::take(&mut *opened)).unwrap_or_default()
    }

    pub async fn request(&self, request: Request) -> Response {
        Handler::handle(&*self.engine, &self.client, request).await
    }

    /// For requests whose only interesting outcome is whether they worked.
    pub async fn simple(&self, request: Request) -> Result<(), String> {
        match self.request(request).await {
            Response::Rejected { reason } => Err(reason),
            Response::Error { message } => Err(message),
            _ => Ok(()),
        }
    }

    pub async fn add_download(
        &self,
        url: String,
        filename: Option<String>,
        mirrors: Vec<String>,
        queue: Option<i64>,
    ) -> Result<u64, String> {
        let request = DownloadRequest { url, filename, mirrors, queue, ..Default::default() };
        match self.request(Request::Enqueue(request)).await {
            Response::Accepted { id } => Ok(id.0),
            Response::Rejected { reason } => Err(reason),
            Response::Error { message } => Err(message),
            other => Err(format!("unexpected reply: {other:?}")),
        }
    }

    /// The way out the engine is currently configured to take.
    ///
    /// Read through the engine rather than kept here, so the window cannot
    /// drift from what downloads are actually doing. A failure to read falls
    /// back to the default, which honours the machine's own configuration.
    /// Takes the address it is about to fetch, because a proxy script
    /// decides per address and cannot be asked without one.
    pub async fn proxy(&self, url: &str) -> idm_engine::proxy::ProxyMode {
        match self.request(Request::GetSettings).await {
            Response::Settings(settings) => idm_engine::proxy::mode_for(&settings, url).await,
            _ => idm_engine::proxy::ProxyMode::default(),
        }
    }

    /// Jobs and settings together, from one instant.
    pub async fn snapshot(&self) -> Result<serde_json::Value, String> {
        let jobs = match self.request(Request::Status { id: None }).await {
            Response::Status { jobs } => jobs,
            Response::Error { message } => return Err(message),
            _ => Vec::new(),
        };
        let settings = match self.request(Request::GetSettings).await {
            Response::Settings(settings) => Some(settings),
            _ => None,
        };
        let (paired, pending) = match self.request(Request::ListPairings).await {
            Response::Pairings { paired, pending } => (paired, pending),
            _ => (Vec::new(), Vec::new()),
        };
        // Here rather than only when the Queues panel is opened, because the
        // numbers on it are what it is for: how many of a queue's downloads
        // are going and how many are held back. Asked for once per tick
        // alongside everything else, so the engine stays the only thing
        // counting them -- it counts a job that has been handed a slot as
        // running, which the window cannot tell from its own job list.
        // Refused when there is no database, which is not an error here.
        let queues = match self.request(Request::ListQueues).await {
            Response::Queues { queues } => queues,
            _ => Vec::new(),
        };

        // Asked for every tick like the rest. A countdown to the machine
        // shutting down is the one thing here the window must never be
        // stale about: it is the only place the person can stop it.
        let armed = match self.request(Request::ArmedAction).await {
            Response::Armed { armed } => armed,
            _ => None,
        };

        Ok(serde_json::json!({
            "jobs": jobs,
            "settings": settings,
            "paired": paired,
            "pending": pending,
            "queues": queues,
            "armed": armed,
        }))
    }

    pub fn history(&self, limit: u32) -> Result<serde_json::Value, String> {
        let Some(store) = &self.store else {
            return Ok(serde_json::json!([]));
        };
        let rows = store.history(limit.min(500)).map_err(|error| error.to_string())?;
        Ok(serde_json::Value::Array(
            rows.into_iter()
                .map(|job| {
                    serde_json::json!({
                        "id": job.id,
                        "url": job.url,
                        "filename": job.filename,
                        "output_dir": job.output_dir,
                        "state": job.state.as_str(),
                        "downloaded": job.downloaded,
                        "total": job.total,
                        // Three fields, not one, because the list, the icon
                        // and the Technical details pane each want a
                        // different one of them. See `idm_engine::failure`.
                        "error": job.error,
                        "error_kind": job.error_kind,
                        "error_summary": job.error_summary,
                        // The page the link was followed from, which is one
                        // of the two places a published hash is looked for.
                        "referrer": job.referrer,
                        "finished_at_ms": job.finished_at_ms,
                        // When to file it under. A download that was stopped
                        // and left never finished, so it has no finish time
                        // and would otherwise group under no date at all.
                        "at_ms": job.finished_at_ms.unwrap_or(job.updated_at_ms),
                        // Whether the engine may still be holding it, which
                        // decides whether History offers to act on it or
                        // points at the Downloads tab.
                        "live": !job.state.is_finished(),
                    })
                })
                .collect(),
        ))
    }

    pub fn clear_history(&self) -> Result<(), String> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        store.clear_history().map(|_| ()).map_err(|error| error.to_string())
    }

    /// Forgets the cancelled and failed rows, leaving finished downloads in
    /// the list.
    pub fn clear_incomplete_history(&self) -> Result<(), String> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        store.clear_incomplete_history().map(|_| ()).map_err(|error| error.to_string())
    }

    /// Forgets one history row. Refused for a job the engine still has live,
    /// so a running download cannot be made to vanish from the Downloads tab
    /// by a stale History-tab click.
    pub fn remove_history_row(&self, row_id: i64) -> Result<(), String> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        match store.job(row_id).map_err(|error| error.to_string())? {
            Some(job) if job.state.is_finished() => {}
            Some(_) => return Err("that download is still going".into()),
            None => return Ok(()),
        }
        store.remove_job(row_id).map(|_| ()).map_err(|error| error.to_string())
    }
}

/// `%USERPROFILE%\Downloads`, which is where a download manager should put
/// things unless told otherwise.
fn default_download_dir() -> std::path::PathBuf {
    std::env::var_os("USERPROFILE")
        .map(std::path::PathBuf::from)
        .map(|home| home.join("Downloads"))
        .unwrap_or_else(std::env::temp_dir)
}
