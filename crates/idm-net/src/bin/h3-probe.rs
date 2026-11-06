//! Finds out what it actually takes to get reqwest onto HTTP/3.
//!
//! Not a test and not shipped behaviour: a diagnostic, written because
//! turning on `http3_prior_knowledge` made our downloads come back as
//! HTTP/1.1 where plain TCP had been getting HTTP/2. Something was
//! silently routing HTTP/3 requests over TCP, and the candidates from
//! reqwest's own issue tracker are a client built with a proxy (which
//! disables h3), a QUIC socket that cannot bind, and a request that has to
//! carry `Version::HTTP_3` itself.
//!
//! Run it against a host known to advertise `alt-svc: h3`:
//!
//! ```text
//! cargo run -p idm-net --bin h3-probe -- https://fonts.gstatic.com/s/roboto/v30/KFOmCnqEu92Fr1Mu4mxK.woff2
//! ```

use std::time::Duration;

#[tokio::main]
async fn main() {
    let url = std::env::args().nth(1).unwrap_or_else(|| {
        "https://fonts.gstatic.com/s/roboto/v30/KFOmCnqEu92Fr1Mu4mxK.woff2".to_owned()
    });
    println!("url: {url}\n");

    // Each case differs from the one before it by exactly one thing, so
    // whichever first reports HTTP/3 names the cause.
    try_it("plain client, no h3 asked for", &url, false, false, false).await;
    try_it("http3_prior_knowledge", &url, true, false, false).await;
    try_it("http3_prior_knowledge + no_proxy", &url, true, true, false).await;
    try_it("http3_prior_knowledge + no_proxy + request version", &url, true, true, true).await;
    try_it("request version only", &url, false, true, true).await;
}

async fn try_it(label: &str, url: &str, prior: bool, no_proxy: bool, set_version: bool) {
    let mut builder = reqwest::Client::builder()
        .user_agent("h3-probe")
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20));
    if no_proxy {
        builder = builder.no_proxy();
    }
    if prior {
        builder = builder.http3_prior_knowledge();
    }

    let client = match builder.build() {
        Ok(client) => client,
        Err(error) => {
            println!("{label:<52} BUILD FAILED: {error}");
            return;
        }
    };

    let mut request = client.get(url);
    if set_version {
        request = request.version(reqwest::Version::HTTP_3);
    }

    match request.send().await {
        Ok(response) => {
            println!("{label:<52} {:?}  {}", response.version(), response.status());
        }
        Err(error) => println!("{label:<52} ERROR: {error}"),
    }
}
