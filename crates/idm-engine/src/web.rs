//! A small web interface, for watching downloads from another room.
//!
//! Off unless somebody switches it on, and narrow on purpose. This is the
//! one part of the program that listens on the network, so every decision
//! here is about what it is *not*.
//!
//! # What it can do
//!
//! List what is downloading, and pause, resume or cancel one. That is the
//! whole of it. There is no route for adding a download, changing a
//! setting, reading the vault, picking a folder or asking the engine to
//! fetch anything — not because those would be refused, though they would
//! be (every request goes to the engine as [`idm_ipc::REMOTE_CLIENT`],
//! which `is_trusted` says no to), but because a route that does not exist
//! cannot be reached by a bug in the one next to it.
//!
//! # The token
//!
//! One secret, thirty-two random bytes, kept in Windows Credential Manager
//! rather than in `settings.json`: the settings file is plain text in the
//! user's profile and gets copied around with it, and this is the only
//! thing standing between the network and somebody's downloads.
//!
//! It is generated once and then kept, so the address stays the same and a
//! bookmark on a phone goes on working. Every request carries it — the
//! page in its address the first time, as a bearer token afterwards — and
//! it is compared in constant time, because a byte-at-a-time comparison
//! leaks how much of a guess was right.
//!
//! # What this is not
//!
//! **There is no encryption.** This is plain HTTP, so anybody who can see
//! the traffic on the network can read it, the token included. That is
//! stated in the README and on the page itself rather than papered over
//! with a self-signed certificate, which would train somebody to click
//! through a browser warning and teach them the wrong lesson. It is for a
//! home network, and it is off by default.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use idm_core::win::credentials::{Secret, Vault, WindowsVault};
use idm_ipc::{ClientInfo, Handler, JobId, Request, Response};

use crate::serve::Engine;

/// The port the interface listens on when nothing else is chosen.
///
/// Stated in the protocol crate, because `Settings` has to read a zero as
/// this number and the two must not drift apart.
pub use idm_ipc::DEFAULT_WEB_PORT as DEFAULT_PORT;

/// Where the token lives in Credential Manager.
///
/// Under the program's own prefix, so it is listed and removed with
/// everything else of ours and never confused with a site password.
/// Built from `credentials::PREFIX` rather than spelled out, because that
/// prefix is what `credentials` lists and scopes by: a target written by
/// hand would be orphaned the moment the prefix changed.
pub fn token_target() -> String {
    format!("{}web-interface", crate::credentials::PREFIX)
}

/// How often the supervisor looks at the setting.
const LOOK: Duration = Duration::from_secs(5);

/// How much of a request body will be read.
///
/// Every route that takes one takes nothing at all, so this is purely a
/// bound on what a stranger can make this process allocate.
const MOST_BODY_BYTES: usize = 4 << 10;

/// The secret, and the engine behind it.
#[derive(Clone)]
struct Shared {
    engine: Arc<Engine>,
    token: Arc<idm_ipc::SessionToken>,
}

/// Reads the token out of the vault, writing a fresh one the first time.
///
/// Kept rather than regenerated on each start, so the address somebody
/// bookmarked on their phone goes on working.
pub fn token(vault: &dyn Vault) -> Result<idm_ipc::SessionToken, String> {
    let target = token_target();
    match vault.read(&target) {
        Ok(Some(found)) if !found.secret.expose().is_empty() => {
            Ok(idm_ipc::SessionToken::from_stored(found.secret.expose().to_owned()))
        }
        Ok(_) => {
            let fresh = idm_ipc::SessionToken::generate();
            vault
                .write(&target, "web", &Secret::new(fresh.as_str()))
                .map_err(|error| error.to_string())?;
            Ok(fresh)
        }
        Err(error) => Err(error.to_string()),
    }
}

/// The address to open the interface at, from another device.
///
/// The local end of a route to somewhere outside, which is how a machine
/// with a wired port, a wireless one and three virtual adapters is asked
/// which of its addresses the rest of the house would reach it on.
/// Nothing is sent: a connected UDP socket is a routing-table lookup and
/// a note of the answer.
///
/// Falls back to `localhost`, which is honest -- it is the one address
/// that definitely works, from this machine only -- rather than guessing.
pub fn where_to_open(port: u16, token: &str) -> String {
    let host = std::net::UdpSocket::bind(("0.0.0.0", 0))
        .and_then(|socket| {
            // Any address outside this machine. Nothing listens there and
            // nothing is sent to it.
            socket.connect(("192.0.2.1", 9))?;
            socket.local_addr()
        })
        .map(|address| address.ip().to_string())
        .unwrap_or_else(|_| "localhost".to_owned());

    format!("http://{host}:{port}/?k={token}")
}

/// Keeps the interface matching the setting, for as long as the app runs.
///
/// A supervisor rather than a one-off start, because the switch is in
/// Settings: somebody turning it off expects the port to close, and
/// somebody turning it on does not expect to have to restart the app.
pub async fn supervise(engine: Arc<Engine>) {
    let mut running: Option<(u16, tokio::task::JoinHandle<()>)> = None;

    loop {
        let settings = engine.settings().await;
        let wanted = settings.web_interface.then(|| settings.web_port_or_default());

        match (&running, wanted) {
            // Already serving the right port.
            (Some((port, task)), Some(want)) if *port == want && !task.is_finished() => {}
            // Switched off, or moved: stop what is there first, so the old
            // port is closed before the new one is asked for.
            _ => {
                if let Some((_, task)) = running.take() {
                    task.abort();
                }
                if let Some(port) = wanted {
                    match start(Arc::clone(&engine), port).await {
                        Ok(task) => running = Some((port, task)),
                        Err(error) => {
                            tracing::error!(%error, port, "could not open the web interface");
                            // Not retried in a tight loop: the usual reason
                            // is a port something else already has, and
                            // hammering it would only fill the log.
                            tokio::time::sleep(LOOK * 6).await;
                        }
                    }
                }
            }
        }

        tokio::time::sleep(LOOK).await;
    }
}

/// Binds the port and serves until the task is dropped.
async fn start(engine: Arc<Engine>, port: u16) -> Result<tokio::task::JoinHandle<()>, String> {
    let vault = WindowsVault;
    let token = Arc::new(token(&vault)?);

    // Every address on the machine: the whole point is reaching it from a
    // phone on the same network. The token is what makes that safe, and
    // the switch in Settings is what makes it deliberate.
    let address = SocketAddr::from((Ipv4Addr::UNSPECIFIED, port));
    let listener =
        tokio::net::TcpListener::bind(address).await.map_err(|error| error.to_string())?;

    tracing::info!(port, "the web interface is listening");
    Ok(serve(engine, listener, token))
}

/// Serves on a listener somebody else bound.
///
/// Split out so a test can take a loopback port of the system's choosing
/// and talk to the real routes. A test that reimplemented the router
/// would prove things about its copy.
pub fn serve(
    engine: Arc<Engine>,
    listener: tokio::net::TcpListener,
    token: Arc<idm_ipc::SessionToken>,
) -> tokio::task::JoinHandle<()> {
    let shared = Shared { engine, token };
    let app = Router::new()
        .route("/", get(page))
        .route("/api/jobs", get(jobs))
        .route("/api/jobs/{id}/{action}", post(act))
        // Anything else, including every route the window has, is not here
        // and never was.
        .fallback(|| async { (StatusCode::NOT_FOUND, "no") })
        .layer(axum::extract::DefaultBodyLimit::max(MOST_BODY_BYTES))
        .with_state(shared);

    tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            tracing::error!(%error, "the web interface stopped");
        }
    })
}

/// The token a request presented, from either place it may carry one.
///
/// The header for everything the page does, and the address for the first
/// load — a browser opening a bookmark cannot send a header it has never
/// been told about.
fn presented(headers: &HeaderMap, query: &std::collections::HashMap<String, String>) -> String {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_owned)
        .or_else(|| query.get("k").cloned())
        .unwrap_or_default()
}

/// Whether a request may proceed.
fn allowed(
    shared: &Shared,
    headers: &HeaderMap,
    query: &std::collections::HashMap<String, String>,
) -> bool {
    shared.token.matches(&presented(headers, query))
}

async fn page(
    State(shared): State<Shared>,
    headers: HeaderMap,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    if !allowed(&shared, &headers, &query) {
        // Deliberately the same body as a wrong path: a stranger scanning
        // the network learns nothing from the difference between "wrong
        // token" and "nothing here".
        return (StatusCode::NOT_FOUND, "no").into_response();
    }
    Html(PAGE).into_response()
}

async fn jobs(
    State(shared): State<Shared>,
    headers: HeaderMap,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    if !allowed(&shared, &headers, &query) {
        return (StatusCode::NOT_FOUND, "no").into_response();
    }

    match shared.engine.handle(&ClientInfo::remote(), Request::Status { id: None }).await {
        Response::Status { jobs } => {
            // Only the fields a progress list needs. The engine says more
            // than that -- where the file is going, what went wrong in
            // full -- and none of it has to leave the machine for a bar to
            // be drawn.
            let shown: Vec<_> = jobs
                .into_iter()
                .map(|job| {
                    serde_json::json!({
                        "id": job.id,
                        "filename": job.filename,
                        "state": job.state,
                        "downloaded": job.downloaded,
                        "total": job.total,
                        "bytes_per_sec": job.bytes_per_sec,
                        "error": job.error,
                    })
                })
                .collect();
            axum::Json(serde_json::json!({ "jobs": shown })).into_response()
        }
        other => {
            tracing::warn!(?other, "unexpected reply for the web interface");
            (StatusCode::INTERNAL_SERVER_ERROR, "no").into_response()
        }
    }
}

async fn act(
    State(shared): State<Shared>,
    Path((id, action)): Path<(u64, String)>,
    headers: HeaderMap,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    if !allowed(&shared, &headers, &query) {
        return (StatusCode::NOT_FOUND, "no").into_response();
    }

    let id = JobId(id);
    // Spelled out rather than parsed into something general: these three
    // are the whole of what the network may ask for, and a list is the
    // clearest way to say so.
    let request = match action.as_str() {
        "pause" => Request::Pause { id },
        "resume" => Request::Resume { id },
        "cancel" => Request::Cancel { id },
        _ => return (StatusCode::NOT_FOUND, "no").into_response(),
    };

    match shared.engine.handle(&ClientInfo::remote(), request).await {
        Response::Accepted { .. } => (StatusCode::OK, "ok").into_response(),
        Response::Rejected { reason } => (StatusCode::FORBIDDEN, reason).into_response(),
        other => {
            tracing::warn!(?other, "unexpected reply for the web interface");
            (StatusCode::INTERNAL_SERVER_ERROR, "no").into_response()
        }
    }
}

/// The whole page: one file, no requests to anywhere else.
///
/// Inline rather than a script and a stylesheet beside it, because every
/// extra file is another route on a server that is meant to have three.
const PAGE: &str = include_str!("web/page.html");

#[cfg(test)]
mod tests {
    use super::*;
    use idm_core::win::credentials::{Credential, Entry, VaultError};

    /// A vault in memory. `idm_core`'s own fake is behind `cfg(test)` in
    /// that crate, so it cannot be reached from here.
    #[derive(Default)]
    struct Pretend(std::sync::Mutex<std::collections::BTreeMap<String, Credential>>);

    impl Vault for Pretend {
        fn write(&self, target: &str, username: &str, secret: &Secret) -> Result<(), VaultError> {
            self.0.lock().unwrap().insert(
                target.to_owned(),
                Credential { username: username.to_owned(), secret: secret.clone() },
            );
            Ok(())
        }
        fn read(&self, target: &str) -> Result<Option<Credential>, VaultError> {
            Ok(self.0.lock().unwrap().get(target).cloned())
        }
        fn remove(&self, target: &str) -> Result<(), VaultError> {
            self.0.lock().unwrap().remove(target);
            Ok(())
        }
        fn list(&self, prefix: &str) -> Result<Vec<Entry>, VaultError> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .iter()
                .filter(|(target, _)| target.starts_with(prefix))
                .map(|(target, held)| Entry {
                    target: target.clone(),
                    username: held.username.clone(),
                })
                .collect())
        }
    }

    #[test]
    fn a_token_is_made_once_and_then_kept() {
        // So a bookmark on somebody's phone goes on working.
        let vault = Pretend::default();
        let first = token(&vault).unwrap();
        let again = token(&vault).unwrap();
        assert_eq!(first.as_str(), again.as_str());
        assert_eq!(first.as_str().len(), 64, "thirty-two bytes as hex");
    }

    #[test]
    fn the_token_is_not_in_the_settings_file() {
        // Stated as a test because it is the reason the vault is involved
        // at all: settings.json is plain text that travels with a profile.
        let settings = idm_ipc::Settings::default();
        let written = serde_json::to_string(&settings).unwrap();
        assert!(!written.contains("token"), "{written}");
    }

    #[test]
    fn the_address_to_open_carries_the_key() {
        let shown = where_to_open(8077, "abc123");
        assert!(shown.starts_with("http://"), "{shown}");
        assert!(shown.ends_with(":8077/?k=abc123"), "{shown}");
    }

    #[test]
    fn a_bearer_header_is_read() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer abc123".parse().unwrap());
        assert_eq!(presented(&headers, &Default::default()), "abc123");
    }

    #[test]
    fn the_address_carries_it_for_the_first_load() {
        // A browser opening a bookmark cannot send a header nobody told it
        // about, so the page itself has to arrive some other way.
        let query = std::collections::HashMap::from([("k".to_owned(), "abc123".to_owned())]);
        assert_eq!(presented(&HeaderMap::new(), &query), "abc123");
    }

    #[test]
    fn a_header_wins_over_the_address() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer header".parse().unwrap());
        let query = std::collections::HashMap::from([("k".to_owned(), "address".to_owned())]);
        assert_eq!(presented(&headers, &query), "header");
    }

    #[test]
    fn nothing_presented_is_not_an_empty_match() {
        // `matches` compares lengths first, so an empty guess cannot match
        // a real token -- but the check is worth having in writing.
        let token = idm_ipc::SessionToken::generate();
        assert!(!token.matches(""));
        assert!(!token.matches("Bearer "));
    }

    #[test]
    fn the_page_says_the_connection_is_not_encrypted() {
        // Somebody using this over a network they do not own should be
        // told by the thing they are looking at, not only by the README.
        assert!(PAGE.to_lowercase().contains("not encrypted"), "the page should say so");
    }

    #[test]
    fn the_page_asks_for_nothing_from_anywhere_else() {
        // One file, no fonts, no CDN. A page on a home network should not
        // be telling somebody else's server who is watching downloads.
        assert!(!PAGE.contains("http://"), "the page fetches something");
        assert!(!PAGE.contains("https://"), "the page fetches something");
    }
}
