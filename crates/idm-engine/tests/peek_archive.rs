//! Listing an archive over HTTP without downloading it.
//!
//! `archive_reading` proves the parser against archives written by another
//! implementation; this proves the half that only exists over a socket —
//! that the tail is fetched, the index located, and the second read taken
//! only when the index was not already in hand.
//!
//! The mock server builds a genuinely valid zip, correct CRCs and all, so
//! these are not reading something shaped only to satisfy this parser.

use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use idm_engine::Engine;
use idm_ipc::{ClientInfo, Handler, Request, Response};

struct Server {
    child: Child,
    base: String,
}

impl Server {
    fn start() -> Self {
        let joined =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/mock-server/server.mjs");
        let canonical = joined.canonicalize().expect("the mock server script");
        let text = canonical.to_string_lossy().into_owned();
        let script = text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned();

        let mut child = Command::new("node")
            .arg(&script)
            .args(["--port", "0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("node should be on PATH");

        let stdout = child.stdout.take().expect("piped stdout");
        let mut line = String::new();
        let read = BufReader::new(stdout).read_line(&mut line).unwrap_or(0);
        let base = (read > 0)
            .then(|| line.rsplit_once(' ').map(|(_, url)| url.trim().to_owned()))
            .flatten()
            .filter(|url| url.starts_with("http://"));

        let Some(base) = base else {
            let mut stderr = String::new();
            if let Some(mut pipe) = child.stderr.take() {
                let _ = pipe.read_to_string(&mut stderr);
            }
            let _ = child.kill();
            panic!("mock server failed to start: {line:?}\n{stderr}");
        };
        Self { child, base }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn desktop() -> ClientInfo {
    ClientInfo { name: "desktop".into(), client_id: None }
}

fn browser() -> ClientInfo {
    ClientInfo { name: idm_ipc::BROWSER_CLIENT.into(), client_id: Some("test".into()) }
}

/// An engine whose state folder is a temporary one.
///
/// `IDM_STATE_DIR` first, and that is not a detail: `Engine::new` opens the
/// durable store, and without the override that is the store belonging to
/// whatever copy of DivinePulse is installed on this machine. A test that
/// migrates or writes to somebody's real download history is not a test,
/// it is damage -- and it is silent, because everything still passes.
///
/// Set rather than restored: these tests are the only thing in this
/// binary, and a restore would race the ones running beside it.
fn scratch() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    // SAFETY: single-threaded setup, before anything reads the variable.
    unsafe { std::env::set_var("IDM_STATE_DIR", dir.path()) };
    dir
}

async fn engine() -> (tempfile::TempDir, std::sync::Arc<Engine>) {
    let dir = scratch();
    let engine = Engine::new(dir.path().join("downloads"), 4);
    (dir, engine)
}

#[tokio::test]
async fn an_archive_lists_its_files_without_being_downloaded() {
    let server = Server::start();
    let (_dir, engine) = engine().await;

    let url = server.url("/archive.zip?files=6&each=256");
    let reply = engine.handle(&desktop(), Request::PeekArchive { url }).await;

    let Response::Archive { entries, truncated, total_uncompressed } = reply else {
        panic!("expected a listing, got {reply:?}");
    };
    assert!(!truncated);
    assert_eq!(entries.len(), 6);

    let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(names[0], "file-0000.bin");
    assert!(entries[3].is_directory, "every fourth entry is a folder: {names:?}");

    // Five files of 256 bytes; the folder contributes nothing.
    assert_eq!(total_uncompressed, 5 * 256);
}

#[tokio::test]
async fn an_archive_comment_does_not_hide_the_index() {
    // The comment is what stops the end record sitting at a fixed offset,
    // and is why the tail is searched backwards rather than read from one
    // known position.
    let server = Server::start();
    let (_dir, engine) = engine().await;

    let url = server.url("/archive.zip?files=3&each=64&comment=packed%20by%20the%20mock");
    let reply = engine.handle(&desktop(), Request::PeekArchive { url }).await;

    match reply {
        Response::Archive { entries, .. } => assert_eq!(entries.len(), 3),
        other => panic!("expected a listing, got {other:?}"),
    }
}

#[tokio::test]
async fn a_large_archive_costs_the_same_as_a_small_one() {
    // The whole point. Five hundred files at four kilobytes each is two
    // megabytes of archive, and the listing reads the index and nothing
    // else -- which also means the second read, for a directory that no
    // longer fits in the tail, is exercised here.
    let server = Server::start();
    let (_dir, engine) = engine().await;

    let url = server.url("/archive.zip?files=500&each=4096");
    let reply = engine.handle(&desktop(), Request::PeekArchive { url }).await;

    let Response::Archive { entries, truncated, .. } = reply else {
        panic!("expected a listing, got {reply:?}");
    };
    assert_eq!(entries.len(), 500);
    assert!(!truncated);
    // Every fourth entry is a folder, so the last one is; checking the
    // name of a file picked by index is how this first went wrong.
    assert_eq!(entries[498].name, "file-0498.bin");
    assert!(entries[499].is_directory, "{}", entries[499].name);
}

#[tokio::test]
async fn something_that_is_not_an_archive_says_so() {
    let server = Server::start();
    let (_dir, engine) = engine().await;

    let url = server.url("/honest?size=65536");
    match engine.handle(&desktop(), Request::PeekArchive { url }).await {
        Response::Rejected { reason } => {
            assert!(reason.contains("zip"), "it should say what it looked for: {reason}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn a_server_that_will_not_send_part_of_a_file_is_refused() {
    // Otherwise "look inside" quietly becomes "download the whole thing",
    // which is the opposite of what it is for.
    let server = Server::start();
    let (_dir, engine) = engine().await;

    let url = server.url("/suffix-ignored?size=1048576");
    match engine.handle(&desktop(), Request::PeekArchive { url }).await {
        Response::Rejected { reason } => {
            assert!(reason.contains("downloading all of it"), "{reason}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn a_browser_cannot_ask_what_is_inside_an_archive() {
    // A fetch of this program's choosing, to an address a page would be
    // naming. That is the shape of request somebody else's server should
    // not be able to make this machine send.
    let (_dir, engine) = engine().await;
    let request = Request::PeekArchive { url: "https://example.com/a.zip".into() };

    match engine.handle(&browser(), request).await {
        Response::Rejected { reason } => assert!(reason.contains("archive"), "{reason}"),
        other => panic!("a browser must not peek, got {other:?}"),
    }
}

#[tokio::test]
async fn an_address_that_is_not_http_is_refused_like_any_other() {
    // The same vetting a download gets, because this is the same kind of
    // act: an address from a client, fetched by this process.
    let (_dir, engine) = engine().await;
    let request = Request::PeekArchive { url: "file:///C:/Windows/System32/config/SAM".into() };

    match engine.handle(&desktop(), request).await {
        Response::Rejected { .. } => {}
        other => panic!("a local path must not be fetched, got {other:?}"),
    }
}
