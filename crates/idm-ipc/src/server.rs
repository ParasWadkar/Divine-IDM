//! The named-pipe endpoint the engine listens on.
//!
//! Every connection must present the session token as its first message
//! before anything else is accepted. The pipe's DACL already restricts it to
//! this user, so the token is depth rather than the primary control — but it
//! also pins down the case where a client holds a stale endpoint file after
//! the engine restarted, which the DACL cannot distinguish.

use std::path::PathBuf;
use std::sync::Arc;

use crate::protocol::{MAX_FRAME_BYTES, ProtocolError, Request, Response, read_frame, write_frame};
use crate::security::{Endpoint, SecurityError, SessionToken, endpoint_path, random_pipe_name};

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("security: {0}")]
    Security(#[from] SecurityError),
    #[error("named pipes are only available on Windows")]
    Unsupported,
}

/// Who is on the other end of a connection.
///
/// The name is self-declared, so it is not an authentication claim -- any
/// local process that can open the pipe could assert any name. It exists so
/// the engine can apply *user preferences* per client kind, such as the
/// browser integration switch. The security boundary is the pipe DACL.
#[derive(Debug, Clone)]
pub struct ClientInfo {
    pub name: String,
}

/// The name the browser native messaging host announces itself with.
pub const BROWSER_CLIENT: &str = "browser-host";

impl ClientInfo {
    pub fn is_browser(&self) -> bool {
        self.name == BROWSER_CLIENT
    }
}

/// What the engine does with a request.
pub trait Handler: Send + Sync + 'static {
    fn handle(
        &self,
        client: &ClientInfo,
        request: Request,
    ) -> impl std::future::Future<Output = Response> + Send;
}

/// A listening endpoint. Dropping it removes the published endpoint file.
pub struct IpcServer {
    pipe_name: String,
    token: SessionToken,
    endpoint_file: PathBuf,
}

impl IpcServer {
    /// Picks an unguessable name and a fresh token, and publishes both.
    pub fn bind() -> Result<Self, ServerError> {
        Self::bind_at(endpoint_path())
    }

    pub fn bind_at(endpoint_file: PathBuf) -> Result<Self, ServerError> {
        let pipe_name = random_pipe_name();
        let token = SessionToken::generate();
        Endpoint::new(pipe_name.clone(), &token).write(&endpoint_file)?;
        Ok(Self { pipe_name, token, endpoint_file })
    }

    pub fn pipe_name(&self) -> &str {
        &self.pipe_name
    }

    pub fn token(&self) -> &SessionToken {
        &self.token
    }

    /// Serves connections until the process ends.
    pub async fn serve<H: Handler>(&self, handler: Arc<H>) -> Result<(), ServerError> {
        platform::serve(self, handler).await
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        // Leaving a stale endpoint file behind would send the next client to a
        // pipe that no longer exists.
        Endpoint::remove(&self.endpoint_file);
    }
}

/// Handles one connection: authenticate, then dispatch until it closes.
///
/// Split out from the transport so the whole conversation can be tested over
/// an in-memory duplex stream, without a pipe or a privileged operation.
pub async fn serve_connection<S, H>(
    stream: &mut S,
    token: &SessionToken,
    handler: Arc<H>,
) -> Result<(), ProtocolError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    H: Handler,
{
    // The first message must be Hello with a valid token. Anything else gets
    // one error and the connection closed -- no retries, so a wrong guess
    // costs a reconnection.
    let first: Request = read_frame(stream, MAX_FRAME_BYTES).await?;
    let info;
    match first {
        Request::Hello { token: presented, client } => {
            if !token.matches(&presented) {
                tracing::warn!(%client, "rejected a client presenting the wrong token");
                let refusal = Response::Error { message: "authentication failed".into() };
                write_frame(stream, &refusal, MAX_FRAME_BYTES).await?;
                return Ok(());
            }
            info = ClientInfo { name: client };
            let welcome = Response::Welcome { version: env!("CARGO_PKG_VERSION").to_owned() };
            write_frame(stream, &welcome, MAX_FRAME_BYTES).await?;
        }
        _ => {
            let refusal = Response::Error { message: "expected hello".into() };
            write_frame(stream, &refusal, MAX_FRAME_BYTES).await?;
            return Ok(());
        }
    }

    loop {
        let request = match read_frame::<_, Request>(stream, MAX_FRAME_BYTES).await {
            Ok(request) => request,
            // A client closing is ordinary; a malformed frame is not, but
            // neither should take the server down.
            Err(ProtocolError::Truncated) => return Ok(()),
            Err(ProtocolError::Malformed(error)) => {
                let message = Response::Error { message: format!("malformed request: {error}") };
                write_frame(stream, &message, MAX_FRAME_BYTES).await?;
                return Ok(());
            }
            Err(error) => return Err(error),
        };

        // Authenticating twice is harmless but pointless; refuse it so the
        // state machine stays obvious.
        let response = match request {
            Request::Hello { .. } => Response::Error { message: "already authenticated".into() },
            other => handler.handle(&info, other).await,
        };
        write_frame(stream, &response, MAX_FRAME_BYTES).await?;
    }
}

#[cfg(windows)]
mod platform {
    use super::{Arc, Handler, IpcServer, ServerError, serve_connection};
    use crate::security::platform::PipeSecurity;
    use tokio::net::windows::named_pipe::ServerOptions;

    pub(super) async fn serve<H: Handler>(
        server: &IpcServer,
        handler: Arc<H>,
    ) -> Result<(), ServerError> {
        let security = PipeSecurity::user_only()?;
        let mut first = true;

        loop {
            // SAFETY: `security` owns the descriptor the attributes point at
            // and outlives this call. `first_pipe_instance` on the first
            // instance stops another process squatting the name.
            let mut pipe = unsafe {
                ServerOptions::new()
                    .first_pipe_instance(first)
                    .create_with_security_attributes_raw(server.pipe_name(), security.as_raw())
            }?;
            first = false;

            pipe.connect().await?;
            let token = server.token().clone();
            let handler = Arc::clone(&handler);
            // One misbehaving client must not stall the others.
            tokio::spawn(async move {
                if let Err(error) = serve_connection(&mut pipe, &token, handler).await {
                    tracing::debug!(%error, "ipc connection ended");
                }
            });
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use super::{Arc, Handler, IpcServer, ServerError};

    pub(super) async fn serve<H: Handler>(
        _server: &IpcServer,
        _handler: Arc<H>,
    ) -> Result<(), ServerError> {
        Err(ServerError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{DownloadRequest, JobId};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Counts what it was asked to do, so tests can assert a request did or
    /// did not reach the engine.
    struct CountingHandler {
        seen: AtomicU32,
    }

    impl Handler for CountingHandler {
        async fn handle(&self, _client: &ClientInfo, request: Request) -> Response {
            self.seen.fetch_add(1, Ordering::SeqCst);
            match request {
                Request::Ping => Response::Pong,
                Request::Enqueue(req) => match req.validate() {
                    Ok(()) => Response::Accepted { id: JobId(1) },
                    Err(reason) => Response::Rejected { reason },
                },
                _ => Response::Error { message: "unsupported".into() },
            }
        }
    }

    fn handler() -> Arc<CountingHandler> {
        Arc::new(CountingHandler { seen: AtomicU32::new(0) })
    }

    /// Drives one conversation over an in-memory pipe and returns the replies.
    async fn converse(
        token: &SessionToken,
        handler: Arc<CountingHandler>,
        requests: Vec<Request>,
    ) -> Vec<Response> {
        let (mut client, mut server) = tokio::io::duplex(64 * 1024);

        let token = token.clone();
        let serving =
            tokio::spawn(async move { serve_connection(&mut server, &token, handler).await });

        let mut replies = Vec::new();
        for request in requests {
            if write_frame(&mut client, &request, MAX_FRAME_BYTES).await.is_err() {
                break;
            }
            match read_frame::<_, Response>(&mut client, MAX_FRAME_BYTES).await {
                Ok(reply) => replies.push(reply),
                Err(_) => break,
            }
        }
        drop(client);
        let _ = serving.await;
        replies
    }

    #[tokio::test]
    async fn a_valid_token_opens_the_connection() {
        let token = SessionToken::generate();
        let handler = handler();
        let replies = converse(
            &token,
            Arc::clone(&handler),
            vec![
                Request::Hello { token: token.as_str().into(), client: "test".into() },
                Request::Ping,
            ],
        )
        .await;

        assert!(matches!(replies.first(), Some(Response::Welcome { .. })), "{replies:?}");
        assert!(matches!(replies.get(1), Some(Response::Pong)), "{replies:?}");
        assert_eq!(handler.seen.load(Ordering::SeqCst), 1, "only the ping should reach the engine");
    }

    #[tokio::test]
    async fn a_wrong_token_reaches_the_engine_with_nothing() {
        let token = SessionToken::generate();
        let handler = handler();
        let replies = converse(
            &token,
            Arc::clone(&handler),
            vec![
                Request::Hello { token: "0".repeat(64), client: "impostor".into() },
                Request::Ping,
            ],
        )
        .await;

        assert!(matches!(replies.first(), Some(Response::Error { .. })), "{replies:?}");
        assert_eq!(
            handler.seen.load(Ordering::SeqCst),
            0,
            "an unauthenticated client must not reach the engine at all"
        );
    }

    #[tokio::test]
    async fn requests_before_hello_are_refused() {
        let token = SessionToken::generate();
        let handler = handler();
        let replies =
            converse(&token, Arc::clone(&handler), vec![Request::Ping, Request::Ping]).await;

        assert!(matches!(replies.first(), Some(Response::Error { .. })), "{replies:?}");
        assert_eq!(handler.seen.load(Ordering::SeqCst), 0, "nothing should have been handled");
    }

    #[tokio::test]
    async fn a_second_hello_is_refused_without_reaching_the_engine() {
        let token = SessionToken::generate();
        let handler = handler();
        let replies = converse(
            &token,
            Arc::clone(&handler),
            vec![
                Request::Hello { token: token.as_str().into(), client: "test".into() },
                Request::Hello { token: token.as_str().into(), client: "test".into() },
            ],
        )
        .await;

        assert!(matches!(replies.get(1), Some(Response::Error { .. })), "{replies:?}");
        assert_eq!(handler.seen.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_hostile_url_is_rejected_rather_than_queued() {
        let token = SessionToken::generate();
        let handler = handler();
        let replies = converse(
            &token,
            Arc::clone(&handler),
            vec![
                Request::Hello { token: token.as_str().into(), client: "test".into() },
                Request::Enqueue(DownloadRequest {
                    url: "file:///C:/Windows/System32/config/SAM".into(),
                    ..Default::default()
                }),
            ],
        )
        .await;

        match replies.get(1) {
            Some(Response::Rejected { reason }) => assert!(reason.contains("http")),
            other => panic!("a file: url should be rejected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_endpoint_file_is_removed_when_the_server_drops() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("endpoint.json");
        {
            let server = IpcServer::bind_at(path.clone()).unwrap();
            assert!(path.exists());
            assert!(server.pipe_name().starts_with(r"\\.\pipe\"));
        }
        // A stale file would send the next client to a pipe that is gone.
        assert!(!path.exists(), "the endpoint file should not outlive the server");
    }
}
