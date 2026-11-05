//! A whole download, with the after-the-bytes pipeline switched on.
//!
//! `finish` is unit tested over verdicts and `scan` over report text, but
//! both of those are the pipeline with nothing plugged into it. This runs a
//! real download to its end with scanning enabled, which is the only way to
//! know that the step actually runs, that it does not hold the job open,
//! and -- the thing most worth proving -- that a clean file still arrives.
//!
//! A feature that fails downloads when the scanner is slow, absent or
//! unhappy would be worse than not having it, and that failure mode is
//! invisible to every test that does not finish a download.

use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use idm_engine::Engine;
use idm_ipc::{ClientInfo, Handler, JobState, Request, Response, Settings};

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

/// An engine whose state folder is a temporary one.
///
/// See the note in `peek_archive`: without `IDM_STATE_DIR`, `Engine::new`
/// opens the store belonging to whatever copy of DivinePulse is installed
/// on this machine.
fn scratch() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    // SAFETY: single-threaded setup, before anything reads the variable.
    unsafe { std::env::set_var("IDM_STATE_DIR", dir.path()) };
    dir
}

/// Waits for a job to stop being in flight, and says how it ended.
async fn settled(engine: &Engine, id: idm_ipc::JobId) -> idm_ipc::JobStatus {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let reply = engine.handle(&desktop(), Request::Status { id: Some(id) }).await;
        let Response::Status { jobs } = reply else {
            panic!("expected a status, got {reply:?}");
        };
        let job = jobs.into_iter().find(|job| job.id == id).expect("the job should be listed");
        if matches!(job.state, JobState::Completed | JobState::Failed) {
            return job;
        }
        assert!(Instant::now() < deadline, "the download never settled: {:?}", job.state);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn a_clean_download_still_finishes_with_scanning_switched_on() {
    let server = Server::start();
    let dir = scratch();
    let downloads = dir.path().join("downloads");
    let engine = Engine::new(downloads.clone(), 4);

    let settings = Settings { scan_downloads: true, ..Settings::default() };
    match engine.handle(&desktop(), Request::SetSettings(settings)).await {
        Response::Settings(settings) => assert!(settings.scan_downloads),
        other => panic!("the setting should stick: {other:?}"),
    }

    let request =
        idm_ipc::DownloadRequest { url: server.url("/honest?size=262144"), ..Default::default() };
    let Response::Accepted { id } = engine.handle(&desktop(), Request::Enqueue(request)).await
    else {
        panic!("the download should be accepted");
    };

    let job = settled(&engine, id).await;
    assert_eq!(
        job.state,
        JobState::Completed,
        "a scanner must never be able to fail an ordinary download: {:?}",
        job.error
    );
    assert_eq!(job.downloaded, 262_144);
    assert!(PathBuf::from(&job.output_dir).join(&job.filename).is_file());
}

#[tokio::test]
async fn the_same_download_finishes_with_scanning_switched_off() {
    // The control. Without it, the test above passing would not say whether
    // the pipeline ran and was harmless or was skipped entirely.
    let server = Server::start();
    let dir = scratch();
    let engine = Engine::new(dir.path().join("downloads"), 4);

    let settings = Settings { scan_downloads: false, ..Settings::default() };
    let _ = engine.handle(&desktop(), Request::SetSettings(settings)).await;

    let request =
        idm_ipc::DownloadRequest { url: server.url("/honest?size=65536"), ..Default::default() };
    let Response::Accepted { id } = engine.handle(&desktop(), Request::Enqueue(request)).await
    else {
        panic!("the download should be accepted");
    };

    assert_eq!(settled(&engine, id).await.state, JobState::Completed);
}
