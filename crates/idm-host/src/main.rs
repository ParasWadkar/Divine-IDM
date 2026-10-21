//! `idm-host` — the bridge between a browser extension and the engine.
//!
//! The browser launches this as a child process and speaks native messaging
//! over stdio; this translates to the engine's named-pipe protocol and back.
//! It is deliberately thin: it holds no state, makes no decisions, and
//! enforces no policy. Everything that matters — whether the extension is
//! paired, whether integration is switched on, where files land — is decided
//! by the engine, which does not trust anything arriving here.
//!
//! That thinness is the point. This process is the part a compromised
//! extension reaches first, so the less it can do, the better.
//!
//! # Exit behaviour
//!
//! The browser closes stdin to say "stop", so a closed pipe is a normal exit
//! rather than a failure. If the engine is not running, the host answers
//! `unavailable` and stays up: the browser should learn that downloads are
//! not being taken over, not have its host crash-loop.

mod framing;

use framing::{Envelope, FromExtension, ToExtension, read_message, write_message};
use idm_ipc::protocol::{DownloadRequest, Request, Response};
use idm_ipc::{BROWSER_CLIENT, IpcClient};

/// Holds the engine connection, reconnecting when it drops.
///
/// The engine may restart, or may not be running when the browser first
/// launches the host, so the connection is made lazily and retried rather
/// than established once at startup.
struct Bridge {
    client_id: Option<String>,
    engine: Option<IpcClient>,
}

impl Bridge {
    fn new() -> Self {
        Self { client_id: None, engine: None }
    }

    /// Connects if needed, announcing the extension's id so the engine knows
    /// which client is asking.
    async fn engine(&mut self) -> Result<&mut IpcClient, String> {
        if self.engine.is_none() {
            let connected = match &self.client_id {
                Some(id) => IpcClient::connect_as(BROWSER_CLIENT, id).await,
                None => IpcClient::connect(BROWSER_CLIENT).await,
            };
            self.engine = Some(connected.map_err(|error| error.to_string())?);
        }
        Ok(self.engine.as_mut().expect("just connected"))
    }

    /// Sends a request, retrying once through a fresh connection.
    ///
    /// A dropped pipe usually means the engine restarted, which should look
    /// like a brief hiccup to the extension rather than a hard failure.
    async fn ask(&mut self, request: Request) -> Result<Response, String> {
        match self.engine().await?.request(request.clone()).await {
            Ok(response) => Ok(response),
            Err(_) => {
                self.engine = None;
                self.engine().await?.request(request).await.map_err(|e| e.to_string())
            }
        }
    }

    async fn handle(&mut self, envelope: Envelope) -> ToExtension {
        // The id is taken from every message, never remembered from a previous
        // one: the process that was greeted has already exited by the time a
        // download arrives, so per-process memory of it is always empty.
        if envelope.client_id != self.client_id {
            self.client_id = envelope.client_id.clone();
            self.engine = None; // reconnect announcing this id
        }

        match envelope.message {
            FromExtension::Hello { label } => {
                match self.ask(Request::RequestPairing { label }).await {
                    Ok(Response::PairingApproved) => {
                        ToExtension::Ready { version: env!("CARGO_PKG_VERSION").to_owned() }
                    }
                    Ok(Response::PairingPending { request_id }) => {
                        ToExtension::PairingPending { request_id }
                    }
                    Ok(Response::Rejected { reason }) => ToExtension::Declined { reason },
                    Ok(other) => ToExtension::Error { message: format!("unexpected: {other:?}") },
                    Err(reason) => ToExtension::Unavailable { reason },
                }
            }

            FromExtension::Pair { label } => {
                match self.ask(Request::RequestPairing { label }).await {
                    Ok(Response::PairingApproved) => ToExtension::PairingApproved,
                    Ok(Response::PairingPending { request_id }) => {
                        ToExtension::PairingPending { request_id }
                    }
                    Ok(Response::Rejected { reason }) => ToExtension::Declined { reason },
                    Ok(other) => ToExtension::Error { message: format!("unexpected: {other:?}") },
                    Err(reason) => ToExtension::Unavailable { reason },
                }
            }

            FromExtension::Download {
                url,
                filename,
                referrer,
                cookies,
                user_agent,
                size_hint,
                mime,
            } => {
                // Passed through unvalidated on purpose: the engine validates,
                // and having one place that decides is better than two places
                // that might disagree.
                let request = DownloadRequest {
                    url,
                    filename,
                    referrer,
                    cookies,
                    user_agent,
                    size_hint,
                    mime,
                };
                match self.ask(Request::Enqueue(request)).await {
                    Ok(Response::Accepted { id }) => ToExtension::Accepted { id: id.0 },
                    Ok(Response::Rejected { reason }) => ToExtension::Declined { reason },
                    Ok(other) => ToExtension::Error { message: format!("unexpected: {other:?}") },
                    Err(reason) => ToExtension::Unavailable { reason },
                }
            }

            FromExtension::GetState => {
                // Probed by enqueueing nothing: the engine's own reply to a
                // pairing request tells us both facts at once, without
                // exposing the settings object to the browser.
                match self.ask(Request::RequestPairing { label: "state probe".into() }).await {
                    Ok(Response::PairingApproved) => {
                        ToExtension::State { integration_enabled: true, paired: true }
                    }
                    Ok(Response::PairingPending { .. }) => {
                        ToExtension::State { integration_enabled: true, paired: false }
                    }
                    Ok(Response::Rejected { .. }) => {
                        ToExtension::State { integration_enabled: false, paired: false }
                    }
                    Ok(other) => ToExtension::Error { message: format!("unexpected: {other:?}") },
                    Err(reason) => ToExtension::Unavailable { reason },
                }
            }

            FromExtension::Ping => match self.ask(Request::Ping).await {
                Ok(Response::Pong) => ToExtension::Pong,
                Ok(other) => ToExtension::Error { message: format!("unexpected: {other:?}") },
                Err(reason) => ToExtension::Unavailable { reason },
            },
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // The browser passes the calling extension origin as the first argument.
    // It is logged for diagnosis only: the real restriction is the
    // `allowed_origins` list in the host manifest, which the browser itself
    // enforces before launching this process at all.
    if let Some(origin) = std::env::args().nth(1) {
        tracing::debug!(%origin, "launched by browser");
    }

    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut bridge = Bridge::new();

    loop {
        let message = match read_message(&mut stdin).await {
            Ok(message) => message,
            // The browser closing stdin is how it says stop.
            Err(framing::FrameError::Closed) => break,
            Err(error) => {
                let reply = ToExtension::Error { message: error.to_string() };
                let _ = write_message(&mut stdout, &reply).await;
                // A malformed frame means the stream is no longer trustworthy,
                // so stop rather than guessing where the next one starts.
                break;
            }
        };

        let reply = bridge.handle(message).await;
        if write_message(&mut stdout, &reply).await.is_err() {
            break;
        }
    }
}
