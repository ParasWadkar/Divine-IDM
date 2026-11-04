//! Windows' own PAC evaluator, driven for real.
//!
//! The plan for this feature said the WinHTTP call would have to be checked
//! by hand, on a network that happens to have a PAC. It does not: pointing
//! it at a script this test serves exercises the whole path — the fetch,
//! the evaluation, the host functions, and the shape of what comes back —
//! and leaves only WPAD's DHCP and DNS discovery untested, which genuinely
//! does need a network configured for it.
//!
//! What makes this worth having is that the parser being right proves
//! nothing on its own. The call could fetch nothing, evaluate nothing, or
//! hand back a string in a shape the parser has never seen, and every unit
//! test would still pass while every download went direct.

#![cfg(windows)]

use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use idm_engine::pac::{AutoConfig, resolve};
use idm_net::client::ProxyMode;

/// The mock server, which serves the script at `/proxy.pac`.
struct Server {
    child: Child,
    base: String,
}

impl Server {
    fn start() -> Self {
        let joined =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/mock-server/server.mjs");
        let canonical = joined.canonicalize().expect("the mock server script should be there");
        // Node cannot resolve a `\\?\` verbatim path as a script.
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

    fn pac_url(&self) -> String {
        format!("{}/proxy.pac", self.base)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test]
async fn a_script_that_names_a_proxy_is_fetched_evaluated_and_believed() {
    let server = Server::start();

    let mode =
        resolve(AutoConfig::Script(server.pac_url()), "https://files.proxied.test/large.bin").await;

    // Every step had to work for this to be the answer: WinHTTP fetched the
    // script, ran `FindProxyForURL`, `shExpMatch` matched the host, and the
    // `PROXY host:port` it returned came back in a shape `pick` understood
    // and turned into an address the transport can use.
    assert_eq!(
        mode,
        ProxyMode::Manual("http://pac-proxy.example.test:8888".into()),
        "the script's own answer should have come back",
    );
}

#[tokio::test]
async fn the_same_script_sends_a_different_address_straight_out() {
    // The other branch, and the one that stops the test above passing for
    // the wrong reason: if the script were never evaluated, every lookup
    // would come back Direct and only this test would notice.
    let server = Server::start();

    let mode = resolve(AutoConfig::Script(server.pac_url()), "https://example.com/file.bin").await;

    assert_eq!(mode, ProxyMode::Direct, "DIRECT is an answer, not a failure");
}

#[tokio::test]
async fn a_script_that_cannot_be_fetched_goes_direct() {
    // A momentarily unreachable script must not turn into a dead
    // application -- and must not fall back to the environment's proxy,
    // which is the one thing choosing automatic configuration rules out.
    let server = Server::start();
    let missing = format!("{}/no-such-script.pac", server.base);

    assert_eq!(
        resolve(AutoConfig::Script(missing), "https://files.proxied.test/x.bin").await,
        ProxyMode::Direct,
    );
}

#[tokio::test]
async fn an_address_the_script_cannot_be_asked_about_goes_direct() {
    // A URL WinHTTP will not evaluate -- no host to hand to the script.
    let server = Server::start();

    assert_eq!(resolve(AutoConfig::Script(server.pac_url()), "not-a-url").await, ProxyMode::Direct,);
}
