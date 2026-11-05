//! The web interface, over a real socket.
//!
//! `web`'s unit tests cover the token and the page. They cannot cover the
//! thing that actually matters: that a stranger who reaches the port
//! without the token is told nothing, and that the routes which do not
//! exist really do not exist. Both of those are properties of the router,
//! so this talks to the router.
//!
//! Bound to a loopback port the system picks, so the test never listens on
//! the network of whoever runs it and never collides with a port in use.

use std::sync::Arc;

use idm_engine::Engine;
use idm_ipc::SessionToken;

/// An engine whose state folder is a temporary one.
///
/// Without `IDM_STATE_DIR`, `Engine::new` opens the store belonging to
/// whatever copy of DivinePulse is installed on this machine.
fn scratch() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    // SAFETY: single-threaded setup, before anything reads the variable.
    unsafe { std::env::set_var("IDM_STATE_DIR", dir.path()) };
    dir
}

struct Served {
    base: String,
    token: String,
    task: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

impl Drop for Served {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn served() -> Served {
    let dir = scratch();
    let engine = Engine::new(dir.path().join("downloads"), 4);
    let token = SessionToken::generate();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = idm_engine::web::serve(engine, listener, Arc::new(token.clone()));
    Served { base, token: token.as_str().to_owned(), task, _dir: dir }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

#[tokio::test]
async fn nothing_is_served_without_the_token() {
    // And nothing is said either: a stranger scanning the network should
    // not learn the difference between "wrong token" and "nothing here".
    let served = served().await;
    let client = client();

    for path in ["/", "/api/jobs"] {
        let reply = client.get(format!("{}{path}", served.base)).send().await.unwrap();
        assert_eq!(reply.status(), 404, "{path}");
    }

    let wrong = client
        .get(format!("{}/api/jobs", served.base))
        .header("Authorization", "Bearer not-the-token")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 404);
}

#[tokio::test]
async fn the_page_and_the_list_arrive_with_it() {
    let served = served().await;
    let client = client();

    // The page takes it in the address, because a browser opening a
    // bookmark cannot send a header nobody told it about.
    let page = client.get(format!("{}/?k={}", served.base, served.token)).send().await.unwrap();
    assert_eq!(page.status(), 200);
    assert!(page.text().await.unwrap().contains("DivinePulse"));

    let list = client
        .get(format!("{}/api/jobs", served.base))
        .header("Authorization", format!("Bearer {}", served.token))
        .send()
        .await
        .unwrap();
    assert_eq!(list.status(), 200);
    let body: serde_json::Value = list.json().await.unwrap();
    assert!(body["jobs"].is_array(), "{body}");
}

#[tokio::test]
async fn there_is_no_route_for_anything_else() {
    // The point of the whole design. These are things the engine would
    // refuse anyway -- the interface speaks as an untrusted client -- but
    // a route that does not exist cannot be reached by a bug in the one
    // beside it.
    let served = served().await;
    let client = client();
    let bearer = format!("Bearer {}", served.token);

    for path in ["/api/settings", "/api/queues", "/api/credentials", "/api/jobs/1/delete"] {
        let got = client
            .post(format!("{}{path}", served.base))
            .header("Authorization", &bearer)
            .send()
            .await
            .unwrap();
        assert_eq!(got.status(), 404, "{path} answered");
    }

    // Including adding a download, which is the one somebody would reach
    // for first and the one that would matter most.
    let adding = client
        .post(format!("{}/api/jobs", served.base))
        .header("Authorization", &bearer)
        .body("https://example.invalid/x.bin")
        .send()
        .await
        .unwrap();
    assert!(adding.status().is_client_error(), "{}", adding.status());
}

#[tokio::test]
async fn an_action_on_a_job_that_is_not_there_is_refused_not_crashed() {
    let served = served().await;
    let reply = client()
        .post(format!("{}/api/jobs/999/pause", served.base))
        .header("Authorization", format!("Bearer {}", served.token))
        .send()
        .await
        .unwrap();
    assert_eq!(reply.status(), 403);
}
