//! Spawns the adversarial mock server for integration tests.
//!
//! The server binds port 0 and prints the port it actually got, so parallel
//! test binaries never collide on a fixed port.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// Resolves the script to a clean absolute path.
///
/// `canonicalize` on Windows returns a `\\?\`-prefixed verbatim path, which
/// Node cannot resolve as a script path — it exits immediately with a module
/// error. The prefix is therefore stripped.
fn script_path() -> PathBuf {
    let joined =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/mock-server/server.mjs");
    let canonical = joined
        .canonicalize()
        .unwrap_or_else(|error| panic!("mock server script not found at {joined:?}: {error}"));

    let text = canonical.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(stripped) => PathBuf::from(stripped),
        None => canonical,
    }
}

pub struct MockServer {
    child: Child,
    base_url: String,
}

impl MockServer {
    /// Starts the server and waits for it to report its bound port.
    pub fn start() -> Self {
        let script = script_path();
        assert!(Path::new(&script).is_file(), "script missing: {script:?}");

        let mut child = Command::new("node")
            .arg(&script)
            .args(["--port", "0"])
            .stdout(Stdio::piped())
            // Captured rather than discarded: when startup fails, whatever the
            // child wrote here is the only explanation available, and nulling
            // it turns a one-line diagnosis into a silent mystery.
            .stderr(Stdio::piped())
            .spawn()
            .expect("node should be on PATH to run the mock server");

        let stdout = child.stdout.take().expect("piped stdout");
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let read = reader.read_line(&mut line).unwrap_or(0);

        // "mock-server listening on http://127.0.0.1:54321"
        let parsed = (read > 0)
            .then(|| line.rsplit_once(' ').map(|(_, url)| url.trim().to_owned()))
            .flatten()
            .filter(|url| url.starts_with("http://"));

        let Some(base_url) = parsed else {
            let mut stderr = String::new();
            if let Some(mut pipe) = child.stderr.take() {
                let _ = pipe.read_to_string(&mut stderr);
            }
            let _ = child.kill();
            panic!(
                "mock server failed to start\n  script: {script:?}\n  stdout: {line:?}\n  stderr: {stderr}"
            );
        };

        Self { child, base_url }
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
