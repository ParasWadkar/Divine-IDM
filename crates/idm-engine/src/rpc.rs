//! An aria2-compatible JSON-RPC endpoint, so other people's tools work here.
//!
//! aria2's RPC is the closest thing this corner of the world has to a
//! standard. AriaNg, Motrix, the browser extensions, the Android remotes and
//! a long tail of scripts all speak it, and Ghost Downloader implements it
//! for exactly that reason. Implementing it is not flattery: it means
//! everything already written against aria2 drives this instead, with no
//! client to install and nothing to port.
//!
//! # Why this is not on the web interface
//!
//! [`crate::web`] is the part that may be reached from another device, and
//! it is deliberately built so that adding a download is not a route that
//! exists. This *is* that route — `addUri` is the whole point of an RPC —
//! so it does not go there.
//!
//! It binds to loopback only, with no option to do otherwise. aria2 has
//! `--rpc-listen-all` and it is a common way for people to expose a
//! downloader to their whole network by accident; there is no equivalent
//! here. Something that wants to reach this from another machine can
//! forward a port and decide for itself that it means to.
//!
//! # What it is allowed to ask for
//!
//! Every call reaches the engine as [`idm_ipc::ClientInfo::rpc`], which
//! `is_trusted` says no to, so the authority check in `serve` refuses
//! anything beyond enqueueing, asking for status and pause/resume/cancel —
//! the same footing as the browser integration. The method table below
//! cannot widen that even if it grows: the refusal is upstream of it.
//!
//! # Where it departs from aria2
//!
//! Stated rather than hidden, because a client that assumes otherwise
//! should fail loudly:
//!
//! * No `changeGlobalOption`, `changeOption`, `shutdown` or anything else
//!   that reconfigures the program. Those are settings, and settings belong
//!   to the person at the window.
//! * `tellStatus` reports the fields a progress display needs. aria2's full
//!   record has several dozen, many of them BitTorrent-specific.
//! * GIDs are this program's job ids written as sixteen hex digits, so they
//!   look like aria2's and round-trip exactly.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use idm_core::win::credentials::{Secret, Vault, WindowsVault};
use idm_ipc::{
    ClientInfo, DownloadRequest, Handler, JobId, JobState, JobStatus, Request, Response,
};
use serde_json::{Value, json};

use crate::serve::Engine;

/// aria2's own default port, because a client that has never been
/// configured tries this one.
///
/// Stated in the protocol crate, because `Settings` has to read a zero as
/// this number and the two must not drift apart.
pub use idm_ipc::DEFAULT_RPC_PORT as DEFAULT_PORT;

/// Where the secret lives in Credential Manager.
///
/// Under the program's own prefix, so it is listed and removed with
/// everything else of ours. Its own entry rather than the web interface's,
/// because the two grant different things and sharing one secret would mean
/// handing a phone on the network the key to `addUri`.
pub fn secret_target() -> String {
    format!("{}rpc-interface", crate::credentials::PREFIX)
}

/// How often the supervisor looks at the setting.
const LOOK: Duration = Duration::from_secs(5);

/// Reads the secret out of the vault, writing a fresh one the first time.
///
/// Kept rather than regenerated on each start, so a client configured once
/// goes on working.
pub fn secret(vault: &dyn Vault) -> Result<String, String> {
    let target = secret_target();
    match vault.read(&target) {
        Ok(Some(found)) if !found.secret.expose().is_empty() => {
            Ok(found.secret.expose().to_owned())
        }
        Ok(_) => {
            let fresh = idm_ipc::SessionToken::generate().as_str().to_owned();
            vault.write(&target, "rpc", &Secret::new(&fresh)).map_err(|error| error.to_string())?;
            Ok(fresh)
        }
        Err(error) => Err(error.to_string()),
    }
}

/// Starts and stops the endpoint as the setting changes.
pub async fn supervise(engine: Arc<Engine>) {
    let mut running: Option<(u16, tokio::task::JoinHandle<()>)> = None;

    loop {
        let settings = engine.settings().await;
        let wanted = settings.rpc_interface.then(|| settings.rpc_port_or_default());

        match (&running, wanted) {
            (Some((port, task)), Some(want)) if *port == want && !task.is_finished() => {}
            _ => {
                if let Some((_, task)) = running.take() {
                    task.abort();
                }
                if let Some(port) = wanted {
                    match start(Arc::clone(&engine), port).await {
                        Ok(task) => running = Some((port, task)),
                        Err(error) => {
                            tracing::error!(%error, port, "could not open the RPC interface");
                            // Not retried tightly: the usual reason is a
                            // port aria2 itself already has, and hammering
                            // it would only fill the log.
                            tokio::time::sleep(LOOK * 6).await;
                        }
                    }
                }
            }
        }

        tokio::time::sleep(LOOK).await;
    }
}

async fn start(engine: Arc<Engine>, port: u16) -> Result<tokio::task::JoinHandle<()>, String> {
    let vault = WindowsVault;
    let secret = Arc::new(secret(&vault)?);

    // Loopback, and no setting says otherwise. This is the difference
    // between this and the web interface, and it is not configurable on
    // purpose: `addUri` reachable from the network is how a downloader
    // becomes somebody else's.
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let listener =
        tokio::net::TcpListener::bind(address).await.map_err(|error| error.to_string())?;

    tracing::info!(port, "the RPC interface is listening on loopback");
    Ok(serve(engine, listener, secret))
}

/// A body larger than this is not a JSON-RPC call, it is a mistake or an
/// attack. aria2 has no documented limit; this one is generous for a
/// `system.multicall` carrying a few hundred addresses.
const MOST_BODY_BYTES: usize = 1 << 20;

/// How many jobs one `tell*` call may return, whatever it asks for.
///
/// `tellWaiting` and `tellStopped` take a count from the caller, and a
/// client asking for a million should get a page rather than the engine
/// serialising its whole history into one response.
const MOST_ROWS: usize = 1000;

#[derive(Clone)]
struct Shared {
    engine: Arc<Engine>,
    secret: Arc<String>,
}

/// Mounts the endpoint on a listener the caller has already bound.
///
/// Taking the listener rather than a port is what lets a test bind
/// `127.0.0.1:0` and talk to the real router. A test that reimplemented the
/// dispatch would prove only that the test agrees with itself.
pub fn serve(
    engine: Arc<Engine>,
    listener: tokio::net::TcpListener,
    secret: Arc<String>,
) -> tokio::task::JoinHandle<()> {
    let app = Router::new()
        // aria2 serves the same thing at both, and clients differ over
        // which they use.
        .route("/jsonrpc", post(call))
        .route("/rpc", post(call))
        .fallback(|| async { (StatusCode::NOT_FOUND, "no") })
        .layer(axum::extract::DefaultBodyLimit::max(MOST_BODY_BYTES))
        .with_state(Shared { engine, secret });

    tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            tracing::error!(%error, "the RPC interface stopped");
        }
    })
}

/// JSON-RPC error codes. The first three are the standard's; `-32098` is
/// what aria2 returns for a bad token and what clients recognise.
mod code {
    pub const INVALID_REQUEST: i64 = -32600;
    pub const NO_SUCH_METHOD: i64 = -32601;
    pub const BAD_PARAMS: i64 = -32602;
    pub const UNAUTHORISED: i64 = -32098;
}

async fn call(State(shared): State<Shared>, body: String) -> impl IntoResponse {
    let Ok(parsed) = serde_json::from_str::<Value>(&body) else {
        return axum::Json(fault(Value::Null, code::INVALID_REQUEST, "not JSON")).into_response();
    };

    // A batch is an array of calls and is answered with an array of
    // replies, in order. Single calls are much the commoner shape, so they
    // are not wrapped and unwrapped to share a path.
    if let Some(batch) = parsed.as_array() {
        if batch.is_empty() {
            return axum::Json(fault(Value::Null, code::INVALID_REQUEST, "empty batch"))
                .into_response();
        }
        let mut out = Vec::with_capacity(batch.len());
        for one in batch {
            out.push(dispatch(&shared, one).await);
        }
        return axum::Json(Value::Array(out)).into_response();
    }

    axum::Json(dispatch(&shared, &parsed).await).into_response()
}

async fn dispatch(shared: &Shared, call: &Value) -> Value {
    let id = call.get("id").cloned().unwrap_or(Value::Null);
    let Some(method) = call.get("method").and_then(Value::as_str) else {
        return fault(id, code::INVALID_REQUEST, "no method");
    };
    let params = call.get("params").and_then(Value::as_array).cloned().unwrap_or_default();

    // `system.multicall` is one call carrying many, and aria2 clients lean
    // on it hard -- AriaNg polls active, waiting and stopped in a single
    // round trip. Each inner call carries its own token.
    if method == "system.multicall" {
        let Some(calls) = params.first().and_then(Value::as_array) else {
            return fault(id, code::BAD_PARAMS, "multicall wants an array of calls");
        };
        let mut out = Vec::with_capacity(calls.len());
        for one in calls.iter().take(MOST_ROWS) {
            let inner_method = one.get("methodName").and_then(Value::as_str).unwrap_or_default();
            let inner_params =
                one.get("params").and_then(Value::as_array).cloned().unwrap_or_default();
            // The standard says each result is wrapped in a one-element
            // array, and a fault is not. aria2 does that too, and clients
            // tell the two apart by shape.
            match answer(shared, inner_method, &inner_params).await {
                Ok(value) => out.push(json!([value])),
                Err((number, message)) => out.push(json!({ "code": number, "message": message })),
            }
        }
        return reply(id, Value::Array(out));
    }

    match answer(shared, method, &params).await {
        Ok(value) => reply(id, value),
        Err((number, message)) => fault(id, number, &message),
    }
}

/// Every method aria2 clients actually call against a plain HTTP downloader.
const METHODS: &[&str] = &[
    "aria2.addUri",
    "aria2.remove",
    "aria2.forceRemove",
    "aria2.pause",
    "aria2.forcePause",
    "aria2.unpause",
    "aria2.tellStatus",
    "aria2.tellActive",
    "aria2.tellWaiting",
    "aria2.tellStopped",
    "aria2.getGlobalStat",
    "aria2.getVersion",
    "system.listMethods",
    "system.multicall",
];

async fn answer(shared: &Shared, method: &str, params: &[Value]) -> Result<Value, (i64, String)> {
    // `listMethods` is the one call that works without a token, because it
    // is how a client discovers whether it is talking to something it
    // understands at all, and it says nothing about this machine.
    if method == "system.listMethods" {
        return Ok(json!(METHODS));
    }

    // aria2 passes the secret as the first parameter, spelled `token:` and
    // then the secret. Compared in constant time for the same reason the
    // web interface does: a byte-at-a-time comparison leaks how much of a
    // guess was right.
    let presented = params
        .first()
        .and_then(Value::as_str)
        .and_then(|first| first.strip_prefix("token:"))
        .unwrap_or_default();
    if !constant_time_eq(presented.as_bytes(), shared.secret.as_bytes()) {
        return Err((code::UNAUTHORISED, "Unauthorized".to_owned()));
    }
    let rest = &params[1..];

    match method {
        "aria2.getVersion" => Ok(json!({
            "version": env!("CARGO_PKG_VERSION"),
            // Named so a client can see what it is really talking to. The
            // aria2 features it would look for are not here, and saying so
            // is better than an empty list that reads as "an old aria2".
            "enabledFeatures": ["DivinePulse"],
        })),

        "aria2.addUri" => {
            let Some(urls) = rest.first().and_then(Value::as_array) else {
                return Err((code::BAD_PARAMS, "addUri wants an array of addresses".to_owned()));
            };
            // aria2 treats the array as mirrors of one file, not as a list
            // of downloads, and so does this: the first is the address and
            // the rest are alternates.
            let mut addresses = urls.iter().filter_map(Value::as_str).map(str::to_owned);
            let Some(url) = addresses.next() else {
                return Err((code::BAD_PARAMS, "no address given".to_owned()));
            };
            let options = rest.get(1);
            let request = DownloadRequest {
                url,
                mirrors: addresses.collect(),
                filename: option(options, "out"),
                referrer: option(options, "referer"),
                user_agent: option(options, "user-agent"),
                ..Default::default()
            };
            match shared.engine.handle(&ClientInfo::rpc(), Request::Enqueue(request)).await {
                Response::Accepted { id } => Ok(Value::String(gid(id))),
                Response::Rejected { reason } => Err((code::BAD_PARAMS, reason)),
                other => Err(unexpected(other)),
            }
        }

        // aria2 distinguishes a polite stop from a forced one. This engine
        // has one kind, so the pair map to the same thing rather than
        // pretending to a difference it does not have.
        "aria2.pause" | "aria2.forcePause" => act(shared, rest, |id| Request::Pause { id }).await,
        "aria2.unpause" => act(shared, rest, |id| Request::Resume { id }).await,
        "aria2.remove" | "aria2.forceRemove" => {
            act(shared, rest, |id| Request::Cancel { id }).await
        }

        "aria2.tellStatus" => {
            let Some(id) = rest.first().and_then(Value::as_str).and_then(parse_gid) else {
                return Err((code::BAD_PARAMS, "tellStatus wants a gid".to_owned()));
            };
            let jobs = status(shared, Some(id)).await?;
            match jobs.into_iter().next() {
                Some(job) => Ok(described(&job)),
                // aria2's own wording for a gid it does not know, which
                // clients match on to drop a row they are polling.
                None => Err((code::BAD_PARAMS, format!("No such download for GID#{}", gid(id)))),
            }
        }

        "aria2.tellActive" => {
            let jobs = status(shared, None).await?;
            Ok(rows(
                jobs.iter()
                    .filter(|job| matches!(job.state, JobState::Running | JobState::Reconnecting)),
            ))
        }
        "aria2.tellWaiting" => {
            let jobs = status(shared, None).await?;
            let waiting =
                jobs.iter().filter(|job| matches!(job.state, JobState::Queued | JobState::Paused));
            Ok(page(waiting, rest))
        }
        "aria2.tellStopped" => {
            let jobs = status(shared, None).await?;
            let stopped = jobs.iter().filter(|job| {
                matches!(job.state, JobState::Completed | JobState::Failed | JobState::Cancelled)
            });
            Ok(page(stopped, rest))
        }

        "aria2.getGlobalStat" => {
            let jobs = status(shared, None).await?;
            let mut active = 0u32;
            let mut waiting = 0u32;
            let mut stopped = 0u32;
            let mut speed = 0f64;
            for job in &jobs {
                match job.state {
                    JobState::Running | JobState::Reconnecting => {
                        active += 1;
                        speed += job.bytes_per_sec;
                    }
                    JobState::Queued | JobState::Paused => waiting += 1,
                    JobState::Completed | JobState::Failed | JobState::Cancelled => stopped += 1,
                }
            }
            // Every number is a string, which looks odd and is what aria2
            // does: its RPC predates anyone worrying about 64-bit integers
            // in JavaScript, and clients parse them as strings.
            Ok(json!({
                "downloadSpeed": (speed as u64).to_string(),
                "uploadSpeed": "0",
                "numActive": active.to_string(),
                "numWaiting": waiting.to_string(),
                "numStopped": stopped.to_string(),
                "numStoppedTotal": stopped.to_string(),
            }))
        }

        _ => Err((code::NO_SUCH_METHOD, format!("No such method: {method}"))),
    }
}

/// Pause, resume and cancel differ only in which request they send.
async fn act(
    shared: &Shared,
    rest: &[Value],
    make: fn(JobId) -> Request,
) -> Result<Value, (i64, String)> {
    let Some(id) = rest.first().and_then(Value::as_str).and_then(parse_gid) else {
        return Err((code::BAD_PARAMS, "wants a gid".to_owned()));
    };
    match shared.engine.handle(&ClientInfo::rpc(), make(id)).await {
        // aria2 answers these with the gid it acted on.
        Response::Accepted { .. } => Ok(Value::String(gid(id))),
        Response::Rejected { reason } => Err((code::BAD_PARAMS, reason)),
        other => Err(unexpected(other)),
    }
}

async fn status(shared: &Shared, id: Option<JobId>) -> Result<Vec<JobStatus>, (i64, String)> {
    match shared.engine.handle(&ClientInfo::rpc(), Request::Status { id }).await {
        Response::Status { jobs } => Ok(jobs),
        other => Err(unexpected(other)),
    }
}

/// `offset` and `num`, as `tellWaiting` and `tellStopped` take them.
///
/// A negative offset counts back from the end in aria2; that is a feature of
/// its own queue ordering and is read here as "from the start", which is the
/// safe reading rather than a wrong one.
fn page<'a>(jobs: impl Iterator<Item = &'a JobStatus>, rest: &[Value]) -> Value {
    let offset = rest.first().and_then(Value::as_i64).unwrap_or(0).max(0) as usize;
    let count = rest.get(1).and_then(Value::as_i64).unwrap_or(MOST_ROWS as i64).max(0) as usize;
    rows(jobs.skip(offset).take(count.min(MOST_ROWS)))
}

fn rows<'a>(jobs: impl Iterator<Item = &'a JobStatus>) -> Value {
    Value::Array(jobs.map(described).collect())
}

/// One job as aria2 describes one.
fn described(job: &JobStatus) -> Value {
    let total = job.total.unwrap_or(0);
    let path = std::path::Path::new(&job.output_dir).join(&job.filename);
    let mut out = json!({
        "gid": gid(job.id),
        "status": aria_state(job.state),
        "totalLength": total.to_string(),
        "completedLength": job.downloaded.to_string(),
        "uploadLength": "0",
        "downloadSpeed": (job.bytes_per_sec as u64).to_string(),
        "uploadSpeed": "0",
        "connections": job.connections.to_string(),
        "dir": job.output_dir,
        // One file per download. A client drawing a file list wants the
        // array even when it holds one entry.
        "files": [{
            "index": "1",
            "path": path.display().to_string(),
            "length": total.to_string(),
            "completedLength": job.downloaded.to_string(),
            "selected": "true",
            "uris": [{ "uri": job.url, "status": "used" }],
        }],
    });
    if let Some(message) = &job.error
        && let Some(object) = out.as_object_mut()
    {
        object.insert("errorMessage".to_owned(), Value::String(message.clone()));
    }
    out
}

/// aria2's five words for what a download is doing.
///
/// `Reconnecting` has no aria2 equivalent and is reported as active, which
/// is what it is: the engine is still working on it and will pick it up
/// again by itself. Calling it `error` would make clients offer to restart
/// something that needs nothing.
fn aria_state(state: JobState) -> &'static str {
    match state {
        JobState::Running | JobState::Reconnecting => "active",
        JobState::Queued => "waiting",
        JobState::Paused => "paused",
        JobState::Completed => "complete",
        JobState::Failed => "error",
        JobState::Cancelled => "removed",
    }
}

/// Job ids as aria2 writes them: sixteen hex digits, no prefix.
fn gid(id: JobId) -> String {
    format!("{:016x}", id.0)
}

fn parse_gid(text: &str) -> Option<JobId> {
    u64::from_str_radix(text, 16).ok().map(JobId)
}

/// One of aria2's per-download options, when the caller sent any.
fn option(options: Option<&Value>, name: &str) -> Option<String> {
    options
        .and_then(|value| value.get(name))
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn unexpected(other: Response) -> (i64, String) {
    tracing::warn!(?other, "unexpected reply for the RPC interface");
    (code::INVALID_REQUEST, "the engine said something unexpected".to_owned())
}

fn reply(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn fault(id: Value, number: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": number, "message": message } })
}

/// Compares without leaking where the first difference is.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right) {
        difference |= a ^ b;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gids_round_trip_and_look_like_aria2s() {
        // Sixteen hex digits is what every client's regular expression
        // expects, and anything that survives the trip out must survive the
        // trip back or pause would act on the wrong job.
        for raw in [0u64, 1, 0xdead_beef, u64::MAX] {
            let written = gid(JobId(raw));
            assert_eq!(written.len(), 16, "{written}");
            assert!(written.chars().all(|c| c.is_ascii_hexdigit()));
            assert_eq!(parse_gid(&written), Some(JobId(raw)));
        }
    }

    #[test]
    fn a_gid_that_is_not_hex_is_refused_rather_than_guessed() {
        assert_eq!(parse_gid("not-a-gid"), None);
        assert_eq!(parse_gid(""), None);
    }

    #[test]
    fn reconnecting_reads_as_active_not_as_an_error() {
        // The distinction the engine draws on purpose. A client told
        // "error" would offer to restart a download that is already coming
        // back on its own.
        assert_eq!(aria_state(JobState::Reconnecting), "active");
        assert_eq!(aria_state(JobState::Failed), "error");
        assert_eq!(aria_state(JobState::Cancelled), "removed");
    }

    #[test]
    fn the_token_is_compared_in_constant_time() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        // A prefix must not pass; length is checked before the loop.
        assert!(!constant_time_eq(b"sec", b"secret"));
    }

    #[test]
    fn listing_the_methods_needs_no_token() {
        // How a client finds out whether it is talking to something it
        // understands. It says nothing about this machine, so requiring the
        // secret first would only make discovery awkward.
        let listed = METHODS;
        assert!(listed.contains(&"aria2.addUri"));
        assert!(listed.contains(&"system.multicall"));
        // Anything that changes how the program is configured is absent,
        // and must stay absent: these are settings, and settings belong to
        // the person at the window.
        for forbidden in ["aria2.changeGlobalOption", "aria2.changeOption", "aria2.shutdown"] {
            assert!(!listed.contains(&forbidden), "{forbidden} must not be offered");
        }
    }

    #[test]
    fn options_are_read_only_when_they_carry_something() {
        let options = json!({ "out": "name.bin", "referer": "" });
        assert_eq!(option(Some(&options), "out").as_deref(), Some("name.bin"));
        // An empty string is aria2's way of saying "unset", and must not
        // become a filename of zero characters.
        assert_eq!(option(Some(&options), "referer"), None);
        assert_eq!(option(Some(&options), "user-agent"), None);
        assert_eq!(option(None, "out"), None);
    }
}
