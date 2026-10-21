//! HTTP clients, tuned, and the lane topology that makes concurrency real.
//!
//! # Two defaults that silently cost most of your bandwidth
//!
//! **The HTTP/2 stream window.** hyper's initial stream window is 64 KiB. A
//! single stream can therefore have at most 64 KiB in flight, which at a 20 ms
//! round trip caps it near 3 MB/s — about a fortieth of a gigabit link — no
//! matter how fast either end is. Nothing reports this; the download is just
//! inexplicably slow on an h2 origin. Both the stream and connection windows
//! are raised here, with adaptive sizing on top.
//!
//! **One client is one connection.** `reqwest` pools per
//! `(scheme, host, port)`, and there is no per-request escape hatch. So asking
//! one `Client` for sixteen concurrent range requests against an h2 origin
//! gives sixteen *streams over a single TCP connection*: one congestion
//! window, one loss-recovery timeline, one flow's share of a shaped path.
//! Multiplexing buys cheap cancellation and zero handshakes — both valuable
//! for work stealing — but it does not buy bandwidth.
//!
//! Genuinely independent flows need genuinely separate clients, which is what
//! a [`LanePool`] is. The recommended shape against an h2 origin is a handful
//! of lanes with several streams each, rather than one lane with many streams
//! or many lanes with one each.
//!
//! # Content coding is disabled on purpose
//!
//! The `gzip`/`brotli`/`deflate`/`zstd` features are deliberately not enabled
//! in `Cargo.toml`, and `Accept-Encoding: identity` is sent explicitly. A
//! content coding on a *ranged* response means the byte offsets address the
//! encoded entity, so every offset the scheduler computes would refer to the
//! wrong place — and transparent decoding would hide that completely.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use reqwest::header::{ACCEPT_ENCODING, HeaderMap, HeaderValue};
use reqwest::{Client, ClientBuilder};

/// Identifies one independent connection lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LaneId(pub u16);

impl std::fmt::Display for LaneId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "lane{}", self.0)
    }
}

#[derive(Debug, Clone)]
pub struct HttpConfig {
    pub user_agent: String,
    /// Concurrent requests allowed per lane. Against h2 these become streams
    /// on one connection, which is why lanes exist at all.
    pub streams_per_lane: usize,
    pub connect_timeout: Duration,
    /// Whole-request ceiling. Generous, because a segment of a large file over
    /// a slow link is legitimately slow.
    pub request_timeout: Duration,
    /// Per-stream in-flight window. The default 64 KiB is the trap described
    /// in the module docs.
    pub h2_stream_window: u32,
    pub h2_connection_window: u32,
    pub h2_max_frame_size: u32,
    pub h2_keep_alive: Duration,
    /// Let hyper size the window from the measured bandwidth-delay product.
    ///
    /// Exposed so its contribution can be separated from the initial window's.
    /// Measurement suggests this, not the initial value, is what actually
    /// lifts single-stream throughput on a high-latency path.
    pub h2_adaptive_window: bool,
    pub pool_idle_timeout: Duration,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            user_agent: concat!("DivineIDM/", env!("CARGO_PKG_VERSION")).to_owned(),
            streams_per_lane: 4,
            connect_timeout: Duration::from_secs(15),
            request_timeout: Duration::from_secs(3600),
            h2_stream_window: 8 << 20,
            h2_connection_window: 32 << 20,
            h2_max_frame_size: 1 << 18,
            h2_keep_alive: Duration::from_secs(20),
            h2_adaptive_window: true,
            pool_idle_timeout: Duration::from_secs(90),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("building the HTTP client failed: {0}")]
    Build(#[source] reqwest::Error),
    #[error("resolving {host}:{port} failed: {source}")]
    Resolve {
        host: String,
        port: u16,
        #[source]
        source: std::io::Error,
    },
    #[error("{host} resolved to no addresses")]
    NoAddresses { host: String },
}

/// What a lane's requests are for, which determines redirect handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Resolving the user's URL to its final location once, up front.
    Resolver,
    /// Fetching byte ranges from the already-resolved URL.
    Segment,
}

fn base_builder(config: &HttpConfig, role: Role) -> ClientBuilder {
    let mut headers = HeaderMap::new();
    // Belt and braces alongside the disabled features: this also tells the
    // server not to encode, rather than merely declining to decode.
    headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));

    let builder = Client::builder()
        .user_agent(config.user_agent.clone())
        .default_headers(headers)
        // Raise the h2 windows well above hyper's 64 KiB default, then let
        // adaptive sizing tune from there.
        .http2_adaptive_window(config.h2_adaptive_window)
        .http2_initial_stream_window_size(config.h2_stream_window)
        .http2_initial_connection_window_size(config.h2_connection_window)
        .http2_max_frame_size(config.h2_max_frame_size)
        .http2_keep_alive_interval(config.h2_keep_alive)
        // Range requests are a stream of small-ish reads whose latency matters
        // for the rate estimate, so do not let Nagle batch them.
        .tcp_nodelay(true)
        // NOTE: SO_RCVBUF is deliberately left alone. Setting it manually
        // disables Windows receive-window autotuning and caps throughput
        // *below* link rate -- the opposite of the intent.
        .pool_idle_timeout(config.pool_idle_timeout)
        .pool_max_idle_per_host(config.streams_per_lane)
        .connect_timeout(config.connect_timeout)
        .timeout(config.request_timeout);

    match role {
        // Follow redirects once, to learn where the file really lives.
        Role::Resolver => builder.redirect(reqwest::redirect::Policy::limited(10)),
        // Segment requests go to the already-pinned URL. A redirect here would
        // mean the server moved the resource mid-download, which must surface
        // as a validator problem rather than be followed silently.
        Role::Segment => builder.redirect(reqwest::redirect::Policy::none()),
    }
}

/// A client that follows redirects, used once to resolve the user's URL.
pub fn redirect_resolver(config: &HttpConfig) -> Result<Client, ClientError> {
    base_builder(config, Role::Resolver).build().map_err(ClientError::Build)
}

/// One independent connection lane.
#[derive(Debug, Clone)]
pub struct Lane {
    pub id: LaneId,
    client: Client,
    pinned: Option<IpAddr>,
}

impl Lane {
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// The edge address this lane is pinned to, if any.
    pub fn pinned_ip(&self) -> Option<IpAddr> {
        self.pinned
    }
}

/// A set of independent lanes, handed out round-robin.
#[derive(Debug)]
pub struct LanePool {
    lanes: Vec<Lane>,
    cursor: AtomicUsize,
}

impl LanePool {
    /// Builds `count` lanes with no address pinning.
    pub fn new(config: &HttpConfig, count: u16) -> Result<Self, ClientError> {
        let count = count.max(1);
        let mut lanes = Vec::with_capacity(count as usize);
        for index in 0..count {
            lanes.push(Lane {
                id: LaneId(index),
                client: base_builder(config, Role::Segment).build().map_err(ClientError::Build)?,
                pinned: None,
            });
        }
        Ok(Self { lanes, cursor: AtomicUsize::new(0) })
    }

    /// Builds lanes pinned to distinct resolved addresses where possible.
    ///
    /// A CDN hostname usually resolves to several edge addresses, and distinct
    /// edges often mean distinct token buckets — so spreading lanes across them
    /// can lift the per-IP ceiling that would otherwise cap the whole download.
    /// Addresses are reused cyclically when there are fewer of them than lanes.
    pub async fn pinned_to_edges(
        config: &HttpConfig,
        host: &str,
        port: u16,
        count: u16,
    ) -> Result<Self, ClientError> {
        let addresses = resolve_all(host, port).await?;
        let count = count.max(1);
        let mut lanes = Vec::with_capacity(count as usize);

        for index in 0..count {
            let address = addresses[index as usize % addresses.len()];
            let client = base_builder(config, Role::Segment)
                .resolve(host, address)
                .build()
                .map_err(ClientError::Build)?;
            lanes.push(Lane { id: LaneId(index), client, pinned: Some(address.ip()) });
        }
        Ok(Self { lanes, cursor: AtomicUsize::new(0) })
    }

    pub fn len(&self) -> usize {
        self.lanes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lanes.is_empty()
    }

    pub fn lanes(&self) -> &[Lane] {
        &self.lanes
    }

    pub fn get(&self, id: LaneId) -> Option<&Lane> {
        self.lanes.iter().find(|lane| lane.id == id)
    }

    /// The next lane, round-robin, so load spreads evenly across flows.
    pub fn next_lane(&self) -> &Lane {
        let index = self.cursor.fetch_add(1, Ordering::Relaxed) % self.lanes.len();
        &self.lanes[index]
    }

    /// How many distinct edge addresses the lanes cover.
    pub fn distinct_edges(&self) -> usize {
        let mut seen: Vec<IpAddr> = self.lanes.iter().filter_map(Lane::pinned_ip).collect();
        seen.sort_unstable();
        seen.dedup();
        seen.len()
    }
}

/// Resolves a host to every address it offers.
pub async fn resolve_all(host: &str, port: u16) -> Result<Vec<SocketAddr>, ClientError> {
    let target = format!("{host}:{port}");
    let addresses: Vec<SocketAddr> = tokio::net::lookup_host(&target)
        .await
        .map_err(|source| ClientError::Resolve { host: host.to_owned(), port, source })?
        .collect();
    if addresses.is_empty() {
        return Err(ClientError::NoAddresses { host: host.to_owned() });
    }
    Ok(addresses)
}

/// Suggested lane count and streams per lane for a target connection count.
///
/// Against an h2 origin, `1 x 16` gives one congestion window and `16 x 1`
/// throws away multiplexing's cheap cancellation. A middle shape — a few lanes
/// with several streams each — gets independent congestion control where it
/// matters and cheap stream cancellation where stealing needs it.
pub fn lane_topology(target_connections: u32) -> (u16, usize) {
    let target = target_connections.max(1);
    let lanes = match target {
        1..=2 => 1,
        3..=6 => 2,
        7..=12 => 3,
        _ => 4,
    };
    let streams = (target as usize).div_ceil(lanes as usize).max(1);
    (lanes, streams)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_h2_windows_are_raised_well_above_hypers_default() {
        // The whole point: hyper's 64 KiB default caps one stream near 3 MB/s
        // at a 20 ms RTT. These must be orders of magnitude larger.
        let config = HttpConfig::default();
        assert!(
            config.h2_stream_window >= 1 << 20,
            "stream window {} is too small to saturate a fast link",
            config.h2_stream_window
        );
        assert!(config.h2_connection_window >= config.h2_stream_window);

        // Sanity-check the arithmetic the default is chosen from: window
        // divided by RTT is the ceiling for one stream.
        let rtt_secs = 0.020;
        let ceiling_bytes_per_sec = f64::from(config.h2_stream_window) / rtt_secs;
        assert!(
            ceiling_bytes_per_sec > 125.0e6,
            "a single stream would cap at {:.0} B/s, below a gigabit",
            ceiling_bytes_per_sec
        );
        // And the 64 KiB default really is as bad as claimed.
        let default_ceiling = 65536.0 / rtt_secs;
        assert!(default_ceiling < 4.0e6, "sanity: hyper's default should be ~3 MB/s");
    }

    #[test]
    fn adaptive_window_sizing_stays_on_by_default() {
        // Load-bearing, and measured. Against a real h2 origin at ~170 ms RTT,
        // one connection:
        //
        //     64 KiB window, adaptive ON  -> 0.75 MB/s
        //     64 KiB window, adaptive OFF -> 0.16 MB/s   (4x slower)
        //     8 MiB  window, adaptive OFF -> 0.64 MB/s
        //
        // So the 64 KiB default genuinely does cripple a stream when it binds,
        // and either adaptive sizing or a large initial window prevents it.
        // Adaptive is the robust one, because it tracks the real
        // bandwidth-delay product instead of guessing a constant.
        let config = HttpConfig::default();
        assert!(config.h2_adaptive_window, "turning this off costs about 4x on a long path");
        assert!(config.h2_stream_window >= 1 << 20, "the belt to adaptive's braces");
    }

    #[test]
    fn clients_build_with_the_tuned_settings() {
        let config = HttpConfig::default();
        redirect_resolver(&config).expect("resolver client should build");
        LanePool::new(&config, 4).expect("lane pool should build");
    }

    #[test]
    fn a_lane_pool_hands_out_lanes_round_robin() {
        let pool = LanePool::new(&HttpConfig::default(), 3).unwrap();
        assert_eq!(pool.len(), 3);
        let first: Vec<LaneId> = (0..6).map(|_| pool.next_lane().id).collect();
        assert_eq!(
            first,
            vec![LaneId(0), LaneId(1), LaneId(2), LaneId(0), LaneId(1), LaneId(2)],
            "load must spread evenly across flows"
        );
    }

    #[test]
    fn lane_counts_are_always_at_least_one() {
        let pool = LanePool::new(&HttpConfig::default(), 0).unwrap();
        assert_eq!(pool.len(), 1, "zero lanes would mean no connections at all");
    }

    #[test]
    fn the_topology_avoids_both_degenerate_shapes() {
        // 1 x N is one congestion window; N x 1 throws away cheap cancellation.
        for target in [4u32, 8, 16, 32] {
            let (lanes, streams) = lane_topology(target);
            assert!(lanes >= 2, "{target} connections should use several lanes, got {lanes}");
            assert!(streams >= 2, "{target} connections should multiplex, got {streams}");
            assert!(
                u32::from(lanes) * streams as u32 >= target,
                "{lanes}x{streams} cannot reach {target} connections"
            );
        }
        // A single connection needs no fan-out at all.
        assert_eq!(lane_topology(1), (1, 1));
    }

    #[tokio::test]
    async fn localhost_resolves_and_pins_lanes() {
        let addresses = resolve_all("localhost", 80).await.expect("localhost should resolve");
        assert!(!addresses.is_empty());

        let pool = LanePool::pinned_to_edges(&HttpConfig::default(), "localhost", 80, 4)
            .await
            .expect("pinned pool should build");
        assert_eq!(pool.len(), 4);
        assert!(
            pool.lanes().iter().all(|lane| lane.pinned_ip().is_some()),
            "every lane should be pinned to an address"
        );
        // Distinct edges are capped by what DNS actually offered.
        assert!(pool.distinct_edges() <= addresses.len().min(4));
        assert!(pool.distinct_edges() >= 1);
    }

    #[tokio::test]
    async fn an_unresolvable_host_is_reported_not_panicked() {
        let error = resolve_all("this-host-does-not-exist.invalid", 443).await.unwrap_err();
        assert!(
            matches!(error, ClientError::Resolve { .. } | ClientError::NoAddresses { .. }),
            "got {error}"
        );
    }
}
