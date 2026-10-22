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
}

impl AppState {
    pub fn new() -> Self {
        let output_dir = default_download_dir();
        Self {
            engine: idm_engine::Engine::new(output_dir, 16),
            store: idm_engine::open_store(),
            client: ClientInfo { name: "desktop".to_owned(), client_id: None },
        }
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
    ) -> Result<u64, String> {
        let request = DownloadRequest { url, filename, ..Default::default() };
        match self.request(Request::Enqueue(request)).await {
            Response::Accepted { id } => Ok(id.0),
            Response::Rejected { reason } => Err(reason),
            Response::Error { message } => Err(message),
            other => Err(format!("unexpected reply: {other:?}")),
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

        Ok(serde_json::json!({
            "jobs": jobs,
            "settings": settings,
            "paired": paired,
            "pending": pending,
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
                        "error": job.error,
                        "finished_at_ms": job.finished_at_ms,
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
}

/// `%USERPROFILE%\Downloads`, which is where a download manager should put
/// things unless told otherwise.
fn default_download_dir() -> std::path::PathBuf {
    std::env::var_os("USERPROFILE")
        .map(std::path::PathBuf::from)
        .map(|home| home.join("Downloads"))
        .unwrap_or_else(std::env::temp_dir)
}
