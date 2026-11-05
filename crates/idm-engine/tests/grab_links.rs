//! Collecting the links off a page, over a real socket.
//!
//! The unit tests in `grab` cover the parsing and the `robots.txt` rules as
//! pure functions, which is where the awkward cases belong. What only
//! exists over a socket is the half these cover: that the origin rule
//! holds, that following stops at one level, that `robots.txt` is actually
//! fetched and obeyed, and that every cap is a cap rather than a comment.

use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use idm_engine::Engine;
use idm_ipc::{ClientInfo, GrabDepth, Handler, Request, Response};

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

    /// Sets what this server's `/robots.txt` will say.
    async fn robots(&self, text: &str) {
        let url =
            format!("{}/_robots?text={}", self.base, urlencoding(text).expect("encodable rules"));
        let status = reqwest::get(&url).await.expect("the mock server should answer").status();
        assert!(status.is_success(), "setting robots.txt answered {status}");
    }
}

/// Just enough percent-encoding for a query value.
fn urlencoding(text: &str) -> Option<String> {
    let mut out = String::new();
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    Some(out)
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
/// whatever copy of DivinePulse is installed on this machine.
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

/// Asks for the links and insists on getting some.
async fn links(
    engine: &Engine,
    url: String,
    follow: GrabDepth,
) -> (Vec<idm_ipc::GrabbedLinkView>, usize, Option<String>) {
    match engine.handle(&desktop(), Request::GrabLinks { url, follow }).await {
        Response::Grabbed { links, pages, note } => (links, pages, note),
        other => panic!("expected a list of links, got {other:?}"),
    }
}

fn addresses(links: &[idm_ipc::GrabbedLinkView]) -> Vec<&str> {
    links.iter().map(|link| link.url.as_str()).collect()
}

#[tokio::test]
async fn a_page_of_links_comes_back_as_a_list() {
    let server = Server::start();
    let (_dir, engine) = engine().await;

    let (found, pages, note) =
        links(&engine, server.url("/site/index.html?files=3&pages=2"), GrabDepth::ThisPage).await;

    assert_eq!(pages, 1, "only the page that was named");
    assert_eq!(note, None, "nothing was cut short");

    let urls = addresses(&found);
    assert!(urls.contains(&server.url("/site/top-0.bin").as_str()), "{urls:?}");
    assert!(urls.contains(&server.url("/site/top-2.bin").as_str()), "{urls:?}");
    assert!(urls.contains(&server.url("/site/logo.png").as_str()), "an image src is a link too");
    assert!(
        urls.contains(&"http://elsewhere.invalid/off.bin"),
        "an off-origin link is collected -- it is only *following* that stays on one origin"
    );
    assert!(!urls.iter().any(|url| url.starts_with("mailto:")), "{urls:?}");
    assert_eq!(
        urls.iter().filter(|url| url.ends_with("top-0.bin")).count(),
        1,
        "the page links to it twice"
    );

    let first = found.iter().find(|link| link.url.ends_with("top-0.bin")).unwrap();
    assert_eq!(first.text, "file number 0", "the anchor's own words");
    assert_eq!(first.from, server.url("/site/index.html?files=3&pages=2"));
}

#[tokio::test]
async fn the_pages_a_page_links_to_are_not_read_unless_asked() {
    let server = Server::start();
    let (_dir, engine) = engine().await;

    let (found, pages, _) =
        links(&engine, server.url("/site/index.html?files=1&pages=2"), GrabDepth::ThisPage).await;

    assert_eq!(pages, 1);
    let urls = addresses(&found);
    assert!(
        urls.iter().any(|url| url.contains("/site/page.html")),
        "the pages are listed as links: {urls:?}"
    );
    assert!(!urls.iter().any(|url| url.contains("sub-")), "but nothing on them was read: {urls:?}");
}

#[tokio::test]
async fn following_goes_one_level_and_no_further() {
    // The whole safety argument of the feature. The followed pages each
    // link to `deeper.html`, and that page's only link is named so that
    // finding it in the list is unambiguous.
    let server = Server::start();
    let (_dir, engine) = engine().await;

    let (found, pages, _) =
        links(&engine, server.url("/site/index.html?files=1&pages=2"), GrabDepth::AndWhatItLinksTo)
            .await;

    assert_eq!(pages, 3, "the index and the two pages it links to");
    let urls = addresses(&found);
    assert!(urls.iter().any(|url| url.ends_with("sub-1-a.bin")), "{urls:?}");
    assert!(urls.iter().any(|url| url.ends_with("sub-2-b.bin")), "{urls:?}");
    assert!(
        urls.iter().any(|url| url.ends_with("/site/deeper.html")),
        "the deeper page is listed as a link, having been found on a page that was read"
    );
    assert!(
        !urls.iter().any(|url| url.contains("must-not-appear")),
        "but it was never fetched, so its links are not here: {urls:?}"
    );
}

#[tokio::test]
async fn only_the_pages_own_origin_is_ever_fetched() {
    // `elsewhere.invalid` does not resolve, so following it would show as a
    // failure rather than as a silent extra request. The point is that the
    // grab finishes cleanly with it listed and unvisited.
    let server = Server::start();
    let (_dir, engine) = engine().await;

    let (found, pages, _) =
        links(&engine, server.url("/site/index.html?files=1&pages=1"), GrabDepth::AndWhatItLinksTo)
            .await;

    assert_eq!(pages, 2, "the index and its one page, and nothing off-origin");
    assert!(addresses(&found).contains(&"http://elsewhere.invalid/off.bin"));
}

#[tokio::test]
async fn a_robots_file_that_forbids_the_page_stops_the_grab() {
    let server = Server::start();
    server.robots("User-agent: *\nDisallow: /site/\n").await;
    let (_dir, engine) = engine().await;

    let request =
        Request::GrabLinks { url: server.url("/site/index.html"), follow: GrabDepth::ThisPage };
    match engine.handle(&desktop(), request).await {
        Response::Rejected { reason } => {
            assert!(reason.contains("robots.txt"), "it should say why: {reason}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn robots_can_forbid_what_would_have_been_followed() {
    // A finer case than a flat refusal: the page asked for is allowed and
    // the ones it links to are not, so the grab succeeds with one page read.
    let server = Server::start();
    server.robots("User-agent: *\nDisallow: /site/page.html\n").await;
    let (_dir, engine) = engine().await;

    let (found, pages, _) =
        links(&engine, server.url("/site/index.html?files=2&pages=2"), GrabDepth::AndWhatItLinksTo)
            .await;

    assert_eq!(pages, 1, "the index only");
    assert!(!addresses(&found).iter().any(|url| url.contains("sub-")));
}

#[tokio::test]
async fn a_group_naming_this_program_is_the_one_obeyed() {
    // `*` is told to go away and we are not, which is the point of having
    // a name in a robots group at all.
    let server = Server::start();
    server
        .robots("User-agent: *\nDisallow: /\n\nUser-agent: divinepulse\nDisallow: /nowhere/\n")
        .await;
    let (_dir, engine) = engine().await;

    let (found, pages, _) =
        links(&engine, server.url("/site/index.html?files=2&pages=0"), GrabDepth::ThisPage).await;
    assert_eq!(pages, 1);
    assert!(!found.is_empty());
}

#[tokio::test]
async fn a_site_with_no_robots_file_is_not_treated_as_a_refusal() {
    // The mock server answers 404 unless a test says otherwise. A missing
    // file means the site has asked for nothing, which is not the same as
    // having asked for silence.
    let server = Server::start();
    let (_dir, engine) = engine().await;

    let (found, _, _) =
        links(&engine, server.url("/site/index.html?files=2&pages=0"), GrabDepth::ThisPage).await;
    assert!(!found.is_empty());
}

#[tokio::test]
async fn a_page_with_more_links_than_the_cap_says_so() {
    let server = Server::start();
    let (_dir, engine) = engine().await;

    let (found, _, note) =
        links(&engine, server.url("/site/many.html?links=2000"), GrabDepth::ThisPage).await;

    assert_eq!(found.len(), idm_engine::grab::MOST_LINKS);
    let note = note.expect("a list that quietly stops looks like a complete list");
    assert!(note.contains("500"), "{note}");
    assert!(note.contains("more than"), "{note}");
}

#[tokio::test]
async fn something_that_is_not_a_page_says_so() {
    let server = Server::start();
    let (_dir, engine) = engine().await;

    let request =
        Request::GrabLinks { url: server.url("/honest?size=4096"), follow: GrabDepth::ThisPage };
    match engine.handle(&desktop(), request).await {
        Response::Rejected { reason } => {
            assert!(reason.contains("not a web page"), "{reason}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn a_links_category_comes_back_with_it() {
    // What the picker groups by, so a page of images and one archive can be
    // narrowed to the archive without reading addresses.
    let server = Server::start();
    let (_dir, engine) = engine().await;

    let (found, _, _) =
        links(&engine, server.url("/site/index.html?files=1&pages=0"), GrabDepth::ThisPage).await;

    let logo = found.iter().find(|link| link.url.ends_with("logo.png")).unwrap();
    assert_eq!(logo.category.as_deref(), Some("Images"));
}

#[tokio::test]
async fn a_browser_cannot_ask_for_the_links_on_a_page() {
    // The firmest of the browser refusals: this makes the engine fetch a
    // series of pages of its own accord, at an address somebody else's page
    // would be choosing.
    let (_dir, engine) = engine().await;
    let request =
        Request::GrabLinks { url: "https://example.com/".into(), follow: GrabDepth::ThisPage };

    match engine.handle(&browser(), request).await {
        Response::Rejected { reason } => assert!(reason.contains("links"), "{reason}"),
        other => panic!("a browser must not grab links, got {other:?}"),
    }
}

#[tokio::test]
async fn an_address_that_is_not_a_web_page_is_refused_before_anything_is_fetched() {
    let (_dir, engine) = engine().await;

    for url in ["file:///C:/Windows/win.ini", "magnet:?xt=urn:btih:0000", "ftp://host/dir/"] {
        let request = Request::GrabLinks { url: url.into(), follow: GrabDepth::ThisPage };
        match engine.handle(&desktop(), request).await {
            Response::Rejected { .. } => {}
            other => panic!("{url} should be refused, got {other:?}"),
        }
    }
}
