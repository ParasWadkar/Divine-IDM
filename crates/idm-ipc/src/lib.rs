//! Local IPC between the engine and its clients.
//!
//! Three things speak this protocol: the CLI, the desktop app, and the
//! browser's native messaging host. The host is the one that shapes the
//! design, because everything it forwards began life in a web page.

pub mod client;
pub mod protocol;
pub mod security;
pub mod server;

pub use client::IpcClient;
pub use protocol::{
    DownloadRequest, JobId, JobState, JobStatus, PairedClient, PendingPairing, PostActionSetting,
    Request, Response, Settings,
};
pub use security::{Endpoint, SessionToken, endpoint_path, random_pipe_name};
pub use server::{BROWSER_CLIENT, ClientInfo, Handler, IpcServer};
