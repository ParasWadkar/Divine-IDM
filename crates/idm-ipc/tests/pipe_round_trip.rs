//! The IPC endpoint over a real named pipe.
//!
//! The unit tests drive the conversation over an in-memory duplex stream,
//! which proves the state machine but not the transport, the DACL, or the
//! endpoint file. These use an actual pipe end to end.

#![cfg(windows)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use idm_ipc::IpcServer;
use idm_ipc::client::IpcClient;
use idm_ipc::protocol::{DownloadRequest, JobId, Request, Response};
use idm_ipc::security::Endpoint;
use idm_ipc::server::{ClientInfo, Handler};

struct TestHandler {
    enqueued: AtomicU32,
}

impl Handler for TestHandler {
    async fn handle(&self, _client: &ClientInfo, request: Request) -> Response {
        match request {
            Request::Ping => Response::Pong,
            Request::Enqueue(req) => match req.validate() {
                Ok(()) => {
                    let id = self.enqueued.fetch_add(1, Ordering::SeqCst) + 1;
                    Response::Accepted { id: JobId(u64::from(id)) }
                }
                Err(reason) => Response::Rejected { reason },
            },
            _ => Response::Error { message: "unsupported".into() },
        }
    }
}

/// Starts a server on a temporary endpoint file and returns its path.
fn start() -> (tempfile::TempDir, std::path::PathBuf, Arc<TestHandler>) {
    let dir = tempfile::tempdir().unwrap();
    let endpoint_file = dir.path().join("endpoint.json");
    let handler = Arc::new(TestHandler { enqueued: AtomicU32::new(0) });

    let server = IpcServer::bind_at(endpoint_file.clone()).expect("bind");
    let serving_handler = Arc::clone(&handler);
    tokio::spawn(async move {
        // Held for the task's lifetime so the endpoint file survives.
        let _ = server.serve(serving_handler).await;
    });

    (dir, endpoint_file, handler)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_connects_and_is_served_over_a_real_pipe() {
    let (_dir, endpoint_file, handler) = start();
    // Give the listener a moment to create its first pipe instance.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let mut client = IpcClient::connect_at(&endpoint_file, "test").await.expect("connect");
    assert!(matches!(client.request(Request::Ping).await.unwrap(), Response::Pong));

    let accepted = client
        .request(Request::Enqueue(DownloadRequest {
            url: "https://example.invalid/a.bin".into(),
            ..Default::default()
        }))
        .await
        .unwrap();
    assert!(matches!(accepted, Response::Accepted { .. }), "{accepted:?}");
    assert_eq!(handler.enqueued.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_with_the_wrong_token_is_refused() {
    let (_dir, endpoint_file, handler) = start();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Same pipe name, wrong token: knowing where the endpoint is must not be
    // enough to use it.
    let mut tampered = Endpoint::read(&endpoint_file).unwrap();
    tampered.token = "0".repeat(64);
    let tampered_file = endpoint_file.with_file_name("tampered.json");
    tampered.write(&tampered_file).unwrap();

    let outcome = IpcClient::connect_at(&tampered_file, "impostor").await;
    assert!(outcome.is_err(), "a wrong token should not connect");
    assert_eq!(
        handler.enqueued.load(Ordering::SeqCst),
        0,
        "nothing should have reached the engine"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn several_clients_are_served_concurrently() {
    // The server creates a fresh pipe instance as soon as one is taken, so a
    // second client must not have to wait for the first to disconnect.
    let (_dir, endpoint_file, handler) = start();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let mut first = IpcClient::connect_at(&endpoint_file, "a").await.expect("first");
    let mut second = IpcClient::connect_at(&endpoint_file, "b").await.expect("second");

    assert!(matches!(first.request(Request::Ping).await.unwrap(), Response::Pong));
    assert!(matches!(second.request(Request::Ping).await.unwrap(), Response::Pong));

    for client in [&mut first, &mut second] {
        client
            .request(Request::Enqueue(DownloadRequest {
                url: "https://example.invalid/a.bin".into(),
                ..Default::default()
            }))
            .await
            .unwrap();
    }
    assert_eq!(handler.enqueued.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connecting_without_an_endpoint_file_fails_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nothing.json");
    let outcome = IpcClient::connect_at(&missing, "test").await;
    assert!(outcome.is_err(), "should report no endpoint rather than hang");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hostile_url_is_refused_over_the_wire() {
    let (_dir, endpoint_file, handler) = start();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let mut client = IpcClient::connect_at(&endpoint_file, "test").await.expect("connect");

    for hostile in [
        "file:///C:/Windows/System32/config/SAM",
        "data:text/plain;base64,QUJD",
        "FILE:///C:/secret",
    ] {
        let reply = client
            .request(Request::Enqueue(DownloadRequest {
                url: hostile.into(),
                ..Default::default()
            }))
            .await
            .unwrap();
        assert!(matches!(reply, Response::Rejected { .. }), "{hostile} gave {reply:?}");
    }
    assert_eq!(handler.enqueued.load(Ordering::SeqCst), 0);
}
