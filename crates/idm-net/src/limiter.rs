//! A speed limit shared by every download.
//!
//! A token bucket that is allowed to go into debt. A reader takes as many
//! tokens as the chunk it just received, immediately, and if that leaves the
//! bucket negative it sleeps off the debt before reading again. Debt rather
//! than waiting for enough tokens first, because a chunk can be larger than
//! the bucket holds: a bucket that insists on having the whole amount before
//! handing any of it out would wait for ever on a chunk bigger than itself.
//!
//! The waiting happens between reads, which is what makes it a real limit
//! rather than a delay. While a reader sleeps it is not polling the
//! response, so hyper stops draining the socket, the TCP receive window
//! closes and the server slows itself down. Nothing is downloaded at full
//! speed and then held back; it simply is not fetched any faster.
//!
//! One limiter for the whole engine. A limit exists so that downloading does
//! not take over the connection, and the connection is shared by everything
//! — a per-download limit of 5 MB/s with four downloads running is 20 MB/s.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::Instant;

/// How much unused allowance may build up while nothing is downloading.
///
/// Small, so a download that starts after an idle spell does not get a
/// multi-second burst at full speed — which is precisely what the person
/// who set a limit was trying to prevent. A quarter of a second's worth.
const BURST: Duration = Duration::from_millis(250);

/// The burst never falls below this, so a very low limit does not reduce
/// every read to a handful of bytes followed by a sleep.
const MIN_BURST_BYTES: f64 = 64.0 * 1024.0;

pub struct RateLimiter {
    /// Bytes per second; zero means no limit.
    rate: AtomicU64,
    bucket: Mutex<Bucket>,
    /// How many reads have had to wait.
    ///
    /// Not for display. A download measures how fast a host can go, and
    /// remembers the answer for next time; if the limit held anything back
    /// while it was measuring, that answer describes the limit rather than
    /// the host, and must not be kept.
    waits: AtomicU64,
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

impl std::fmt::Debug for RateLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RateLimiter")
            .field("bytes_per_sec", &self.rate())
            .field("waits", &self.waits())
            .finish()
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new(0)
    }
}

impl RateLimiter {
    /// A limiter at `bytes_per_sec`, or none at all for zero.
    pub fn new(bytes_per_sec: u64) -> Self {
        Self {
            rate: AtomicU64::new(bytes_per_sec),
            bucket: Mutex::new(Bucket { tokens: 0.0, last: Instant::now() }),
            waits: AtomicU64::new(0),
        }
    }

    pub fn rate(&self) -> u64 {
        self.rate.load(Ordering::Relaxed)
    }

    pub fn waits(&self) -> u64 {
        self.waits.load(Ordering::Relaxed)
    }

    /// Changes the limit, taking effect from the next read.
    ///
    /// The bucket is emptied, so raising a limit cannot release a burst of
    /// allowance saved up under the old one, and lowering it takes hold at
    /// once rather than after the old allowance is spent.
    pub async fn set_rate(&self, bytes_per_sec: u64) {
        self.rate.store(bytes_per_sec, Ordering::Relaxed);
        let mut bucket = self.bucket.lock().await;
        bucket.tokens = 0.0;
        bucket.last = Instant::now();
    }

    /// Accounts for `bytes` just received, sleeping if they were too many.
    ///
    /// Free when there is no limit: the rate is checked before the lock, so
    /// an unlimited download never contends for it.
    pub async fn take(&self, bytes: usize) {
        let rate = self.rate();
        if rate == 0 || bytes == 0 {
            return;
        }

        let wait = {
            let mut bucket = self.bucket.lock().await;
            let now = Instant::now();
            let elapsed = now.saturating_duration_since(bucket.last).as_secs_f64();
            bucket.last = now;

            let rate = rate as f64;
            let ceiling = (rate * BURST.as_secs_f64()).max(MIN_BURST_BYTES);
            bucket.tokens = (bucket.tokens + elapsed * rate).min(ceiling);
            bucket.tokens -= bytes as f64;

            (bucket.tokens < 0.0).then(|| Duration::from_secs_f64(-bucket.tokens / rate))
        };

        if let Some(wait) = wait {
            self.waits.fetch_add(1, Ordering::Relaxed);
            tokio::time::sleep(wait).await;
        }
    }
}

/// Reads a rate such as `5M`, `500K` or `1048576`, in bytes per second.
///
/// Binary multiples, as everywhere else in this program. Zero, `0` or
/// `off` means no limit.
pub fn parse_rate(text: &str) -> Result<u64, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("off") {
        return Ok(0);
    }
    let lower = trimmed.to_ascii_lowercase();
    let body = lower.trim_end_matches("/s").trim_end_matches("b");
    let (digits, multiplier) = match body.chars().last() {
        Some('k') => (&body[..body.len() - 1], 1u64 << 10),
        Some('m') => (&body[..body.len() - 1], 1u64 << 20),
        Some('g') => (&body[..body.len() - 1], 1u64 << 30),
        _ => (body, 1),
    };
    let value: f64 =
        digits.trim().parse().map_err(|_| format!("{text} is not a rate; try 5M or 500K"))?;
    if !value.is_finite() || value < 0.0 {
        return Err(format!("{text} is not a rate; try 5M or 500K"));
    }
    Ok((value * multiplier as f64) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds `total` bytes through in `chunk`-sized reads and reports how
    /// long it took, on a paused clock so the answer is exact.
    async fn time_to_pass(limiter: &RateLimiter, total: usize, chunk: usize) -> Duration {
        let started = Instant::now();
        let mut sent = 0;
        while sent < total {
            let piece = chunk.min(total - sent);
            limiter.take(piece).await;
            sent += piece;
        }
        started.elapsed()
    }

    #[tokio::test(start_paused = true)]
    async fn throughput_follows_the_limit() {
        // 4 MiB at 1 MiB/s should take about four seconds. Not exactly: the
        // first quarter-second burst is allowed, which is the point of it.
        let limiter = RateLimiter::new(1 << 20);
        let took = time_to_pass(&limiter, 4 << 20, 16 << 10).await;
        let seconds = took.as_secs_f64();
        assert!((3.5..=4.1).contains(&seconds), "took {seconds:.2}s");
    }

    #[tokio::test(start_paused = true)]
    async fn no_limit_costs_nothing() {
        let limiter = RateLimiter::new(0);
        let took = time_to_pass(&limiter, 64 << 20, 16 << 10).await;
        assert_eq!(took, Duration::ZERO);
        assert_eq!(limiter.waits(), 0, "an unlimited reader should never wait");
    }

    #[tokio::test(start_paused = true)]
    async fn a_chunk_larger_than_the_bucket_still_goes_through() {
        // The reason for debt: a bucket that waited until it held the whole
        // chunk would never release one bigger than its own capacity.
        let limiter = RateLimiter::new(64 << 10);
        let took = time_to_pass(&limiter, 1 << 20, 1 << 20).await;
        // One 1 MiB read at 64 KiB/s is sixteen seconds of debt.
        let seconds = took.as_secs_f64();
        assert!((15.0..=16.5).contains(&seconds), "took {seconds:.2}s");
    }

    #[tokio::test(start_paused = true)]
    async fn an_idle_spell_does_not_save_up_a_burst() {
        // Someone who sets a limit does not want a download that started
        // after a pause to run flat out for several seconds.
        let limiter = RateLimiter::new(1 << 20);
        tokio::time::sleep(Duration::from_secs(60)).await;
        limiter.take(0).await;

        let took = time_to_pass(&limiter, 2 << 20, 16 << 10).await;
        let seconds = took.as_secs_f64();
        assert!(seconds >= 1.6, "a minute of idling bought too much: {seconds:.2}s");
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_rate_takes_effect_at_once() {
        let limiter = RateLimiter::new(1 << 20);
        limiter.set_rate(4 << 20).await;
        let took = time_to_pass(&limiter, 4 << 20, 16 << 10).await;
        let seconds = took.as_secs_f64();
        assert!((0.8..=1.1).contains(&seconds), "took {seconds:.2}s");

        limiter.set_rate(0).await;
        let took = time_to_pass(&limiter, 64 << 20, 16 << 10).await;
        assert_eq!(took, Duration::ZERO, "switching the limit off should be immediate");
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_readers_share_one_limit() {
        // The reason it is global: four downloads under a 1 MiB/s limit
        // should between them take 1 MiB/s, not 4.
        let limiter = std::sync::Arc::new(RateLimiter::new(1 << 20));
        let started = Instant::now();

        let mut tasks = Vec::new();
        for _ in 0..4 {
            let limiter = std::sync::Arc::clone(&limiter);
            tasks.push(tokio::spawn(async move {
                time_to_pass(&limiter, 1 << 20, 16 << 10).await;
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }

        let seconds = started.elapsed().as_secs_f64();
        assert!((3.5..=4.2).contains(&seconds), "4 MiB across four readers took {seconds:.2}s");
    }

    #[tokio::test(start_paused = true)]
    async fn waiting_is_counted_so_a_throttled_measurement_can_be_discarded() {
        let limiter = RateLimiter::new(64 << 10);
        assert_eq!(limiter.waits(), 0);
        time_to_pass(&limiter, 1 << 20, 16 << 10).await;
        assert!(limiter.waits() > 0);
    }

    #[test]
    fn rates_are_read_the_way_people_write_them() {
        assert_eq!(parse_rate("5M").unwrap(), 5 << 20);
        assert_eq!(parse_rate("500k").unwrap(), 500 << 10);
        assert_eq!(parse_rate("1.5MB/s").unwrap(), (1.5 * (1u64 << 20) as f64) as u64);
        assert_eq!(parse_rate("2G").unwrap(), 2 << 30);
        assert_eq!(parse_rate("1048576").unwrap(), 1 << 20);
        assert_eq!(parse_rate("off").unwrap(), 0);
        assert_eq!(parse_rate("0").unwrap(), 0);
        assert!(parse_rate("fast").is_err());
        assert!(parse_rate("-5M").is_err());
    }
}
