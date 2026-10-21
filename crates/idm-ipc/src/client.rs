//! Connecting to a running engine.
//!
//! Used by the CLI, the desktop app, and the native messaging host. Finding
//! the endpoint means reading a file under the user's own profile, which is
//! also how the token is obtained — a process that cannot read that file
//! cannot talk to the engine even if it guesses the pipe name.

use std::path::Path;
use std::time::Duration;

use crate::protocol::{MAX_FRAME_BYTES, ProtocolError, Request, Response, read_frame, write_frame};
use crate::security::{Endpoint, SecurityError, endpoint_path};

/// How long to keep retrying while every pipe instance is busy.
const BUSY_RETRY_FOR: Duration = Duration::from_secs(5);
const BUSY_RETRY_EVERY: Duration = Duration::from_millis(25);

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol: {0}")]
    Protocol(#[from] ProtocolError),
    #[error("security: {0}")]
    Security(#[from] SecurityError),
    #[error("the engine refused the connection: {0}")]
    Refused(String),
    #[error("named pipes are only available on Windows")]
    Unsupported,
}

/// An authenticated connection to the engine.
pub struct IpcClient {
    #[cfg(windows)]
    pipe: tokio::net::windows::named_pipe::NamedPipeClient,
}

impl IpcClient {
    /// Connects using the endpoint the engine published.
    pub async fn connect(client_name: &str) -> Result<Self, ClientError> {
        Self::connect_at(&endpoint_path(), client_name).await
    }

    pub async fn connect_at(endpoint_file: &Path, client_name: &str) -> Result<Self, ClientError> {
        let endpoint = Endpoint::read(endpoint_file)?;
        platform::connect(&endpoint, client_name).await
    }

    /// Sends one request and waits for its reply.
    pub async fn request(&mut self, request: Request) -> Result<Response, ClientError> {
        platform::request(self, request).await
    }
}

#[cfg(windows)]
mod platform {
    use super::{
        BUSY_RETRY_EVERY, BUSY_RETRY_FOR, ClientError, Endpoint, IpcClient, MAX_FRAME_BYTES,
        Request, Response, read_frame, write_frame,
    };
    use tokio::net::windows::named_pipe::ClientOptions;
    use windows::Win32::Foundation::ERROR_PIPE_BUSY;

    pub(super) async fn connect(
        endpoint: &Endpoint,
        client_name: &str,
    ) -> Result<IpcClient, ClientError> {
        let deadline = std::time::Instant::now() + BUSY_RETRY_FOR;
        let pipe = loop {
            match ClientOptions::new().open(&endpoint.pipe_name) {
                Ok(pipe) => break pipe,
                // Every instance is serving someone else; the server creates
                // another as soon as one is taken, so this clears quickly.
                Err(error)
                    if error.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32)
                        && std::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(BUSY_RETRY_EVERY).await;
                }
                Err(error) => return Err(error.into()),
            }
        };

        let mut client = IpcClient { pipe };
        let hello =
            Request::Hello { token: endpoint.token.clone(), client: client_name.to_owned() };
        write_frame(&mut client.pipe, &hello, MAX_FRAME_BYTES).await?;
        match read_frame::<_, Response>(&mut client.pipe, MAX_FRAME_BYTES).await? {
            Response::Welcome { .. } => Ok(client),
            Response::Error { message } => Err(ClientError::Refused(message)),
            other => Err(ClientError::Refused(format!("unexpected greeting: {other:?}"))),
        }
    }

    pub(super) async fn request(
        client: &mut IpcClient,
        request: Request,
    ) -> Result<Response, ClientError> {
        write_frame(&mut client.pipe, &request, MAX_FRAME_BYTES).await?;
        Ok(read_frame(&mut client.pipe, MAX_FRAME_BYTES).await?)
    }
}

#[cfg(not(windows))]
mod platform {
    use super::{ClientError, Endpoint, IpcClient, Request, Response};

    pub(super) async fn connect(_: &Endpoint, _: &str) -> Result<IpcClient, ClientError> {
        Err(ClientError::Unsupported)
    }

    pub(super) async fn request(_: &mut IpcClient, _: Request) -> Result<Response, ClientError> {
        Err(ClientError::Unsupported)
    }
}
