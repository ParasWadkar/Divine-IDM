//! Whether a configured proxy is actually the way out.
//!
//! Building a client with a proxy set proves nothing on its own: reqwest
//! accepts the address either way, and a mistake surfaces as every download
//! quietly connecting directly. So the proxy in these tests is a real
//! listener, and the address asked for is one that cannot be reached by any
//! other route — `.invalid` is reserved by RFC 2606 and resolves nowhere. A
//! response arriving at all is therefore proof that the request went through
//! the proxy rather than past it.

use std::time::Duration;

use idm_net::client::{HttpConfig, ProxyMode, redirect_resolver};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A forward proxy that answers one request and reports its request line.
///
/// Enough of one for this purpose. A client talking to a forward proxy puts
/// the whole URL in the request line — absolute-form, where it would
/// normally send just the path — so that one line says whether we were
/// treated as a proxy or as the origin.
async fn one_shot_proxy() -> (String, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("should bind a port");
    let address = format!("http://{}", listener.local_addr().unwrap());

    let handle = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("a connection should arrive");

        // Up to the end of the headers, which is both all this needs to see
        // and the point at which the client has finished asking.
        let mut seen = Vec::new();
        let mut buffer = [0u8; 1024];
        while !seen.windows(4).any(|window| window == b"\r\n\r\n") {
            match socket.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => seen.extend_from_slice(&buffer[..read]),
            }
        }

        let body = b"proxied";
        let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
        let _ = socket.write_all(head.as_bytes()).await;
        let _ = socket.write_all(body).await;
        let _ = socket.flush().await;

        String::from_utf8_lossy(&seen).lines().next().unwrap_or_default().to_owned()
    });

    (address, handle)
}

fn config(proxy: ProxyMode) -> HttpConfig {
    HttpConfig {
        proxy,
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(5),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_manual_proxy_is_the_way_out() {
    let (proxy, served) = one_shot_proxy().await;
    let client = redirect_resolver(&config(ProxyMode::Manual(proxy))).expect("should build");

    let response = client
        .get("http://unreachable.invalid/file.bin")
        .send()
        .await
        .expect("the proxy should have answered for a host that resolves nowhere");

    assert!(response.status().is_success());
    assert_eq!(response.text().await.unwrap(), "proxied");

    let request_line = served.await.unwrap();
    assert!(
        request_line.contains("http://unreachable.invalid/file.bin"),
        "a forward proxy is asked with the whole URL, got {request_line:?}"
    );
}

#[tokio::test]
async fn an_address_that_is_not_a_proxy_is_refused_while_the_client_is_built() {
    // Rather than at the first request, by which time a download is already
    // under way and the failure looks like the server's fault.
    let error = redirect_resolver(&config(ProxyMode::Manual("not a proxy".into())))
        .expect_err("should refuse");
    assert!(format!("{error}").contains("not a usable proxy address"), "{error}");
}

#[tokio::test]
async fn direct_reaches_nothing_it_could_not_reach_itself() {
    // A weaker claim than it looks, and deliberately stated as such: what
    // `Direct` is really for is overriding `HTTP_PROXY` in the environment,
    // and that cannot be checked here without setting a process-global
    // variable every other test in this binary would also see. What this
    // does rule out is `Direct` acquiring a proxy from anywhere at all.
    let (_proxy, _served) = one_shot_proxy().await;
    let client = redirect_resolver(&config(ProxyMode::Direct)).expect("should build");

    assert!(
        client.get("http://unreachable.invalid/file.bin").send().await.is_err(),
        "Direct must not reach a host that resolves nowhere"
    );
}
