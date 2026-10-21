//! Fetching one claim's bytes and handing them to the writer.
//!
//! This is where the scheduler's decisions meet an actual socket. The loop is
//! small on purpose; the interesting parts are the three things it must get
//! right.
//!
//! **Stopping when stolen from.** After each flush the claim's soft end is
//! re-read. When the cursor reaches it the body is simply dropped, which on
//! HTTP/2 and HTTP/3 sends `RST_STREAM`/`STOP_SENDING` and leaves the
//! connection pooled. No cancellation channel, no signal: one atomic load.
//!
//! **Backpressure before buffering.** A permit for the bytes is taken *before*
//! they are queued, and released only once the write has been issued. When the
//! budget is empty the loop stops polling the body, hyper stops reading the
//! socket, the TCP window closes and the server slows down.
//!
//! **Trusting `Content-Range`, not the request.** The range actually delivered
//! comes from the response. A server may legally return less than was asked
//! for, and the residue has to be requeued rather than assumed present.

use std::time::{Duration, Instant};

use bytes::BytesMut;
use idm_core::ByteRange;
use idm_core::sched::LiveClaim;
use idm_core::sched::probe::{ProbeOutcome, classify};
use idm_core::writer::{WriteJob, WriterHandle};
use reqwest::header::{COOKIE, IF_MATCH, RANGE, REFERER};
use tokio_stream::StreamExt;

use crate::budget::AsyncBudget;
use crate::client::Lane;
use crate::facts::OwnedFacts;

/// Accumulate to this before handing bytes to the writer.
///
/// hyper yields chunks of a few tens of kilobytes. Forwarding each one
/// separately would multiply channel traffic and heap churn by an order of
/// magnitude for no benefit, since the writer coalesces again anyway.
pub const DEFAULT_CHUNK_TARGET: usize = 1 << 20;

/// Flush a partial chunk after this long, so a slow connection still reports
/// progress and still advances its rate estimate.
pub const DEFAULT_PARTIAL_FLUSH: Duration = Duration::from_millis(50);

#[derive(Debug, Clone)]
pub struct FetchConfig {
    pub chunk_target: usize,
    pub partial_flush_after: Duration,
}

impl Default for FetchConfig {
    fn default() -> Self {
        Self { chunk_target: DEFAULT_CHUNK_TARGET, partial_flush_after: DEFAULT_PARTIAL_FLUSH }
    }
}

/// Everything needed to ask for one range.
#[derive(Debug, Clone, Default)]
pub struct SegmentRequest<'a> {
    pub url: &'a str,
    pub generation: u64,
    /// A **strong** ETag. Sending this makes a changed representation come back
    /// as a cheap `412` rather than N full-body `200`s, and it is the mechanism
    /// that stops two server versions being spliced into one file.
    pub if_match: Option<&'a str>,
    pub referrer: Option<&'a str>,
    /// Forwarded from the browser so cookie-gated downloads work.
    pub cookies: Option<&'a str>,
}

/// Why the fetch loop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchStop {
    /// Reached the claim's soft end.
    Complete,
    /// The soft end was lowered mid-flight: the coordinator gave the tail away.
    StolenFrom,
    /// The body ended before the soft end, so the residue must be requeued.
    ServerEnded,
}

#[derive(Debug, Clone)]
pub struct Fetched {
    /// What was actually handed to the writer.
    pub written: ByteRange,
    pub stop: FetchStop,
    pub facts: OwnedFacts,
}

#[derive(Debug)]
pub enum FetchOutcome {
    // Boxed to keep the variants a similar size: `Fetched` carries the whole
    // OwnedFacts, which would otherwise make every `Refused` pay for it too.
    Fetched(Box<Fetched>),
    /// The probe table said not to proceed with this response. The caller acts
    /// on the outcome: back off, re-resolve, restart, or fall back to a single
    /// stream.
    Refused {
        outcome: ProbeOutcome,
        facts: Box<OwnedFacts>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("request failed: {0}")]
    Request(#[source] reqwest::Error),
    #[error("response body failed after {written} bytes: {source}")]
    Body {
        written: u64,
        #[source]
        source: reqwest::Error,
    },
    #[error("handing bytes to the writer failed: {0}")]
    Write(#[source] std::io::Error),
    #[error("range arithmetic failed: {0}")]
    Range(#[from] idm_core::ExtentError),
}

/// Fetches whatever remains of `claim` and writes it.
///
/// Only a genuine ranged response proceeds here. Anything else — a server that
/// ignored `Range`, a content coding, a throttle, an expired signature — comes
/// back as [`FetchOutcome::Refused`] for the caller to handle, because those
/// need download-wide decisions rather than per-claim ones.
pub async fn fetch_claim(
    lane: &Lane,
    request: &SegmentRequest<'_>,
    claim: &LiveClaim,
    writer: &WriterHandle,
    budget: &AsyncBudget,
    config: &FetchConfig,
) -> Result<FetchOutcome, FetchError> {
    let wanted = ByteRange::try_new(claim.cursor(), claim.end_soft())?;
    if wanted.is_empty() {
        // Already satisfied, usually because a steal landed between the
        // coordinator choosing this claim and the fetcher starting.
        return Ok(FetchOutcome::Fetched(Box::new(Fetched {
            written: ByteRange::empty_at(wanted.start()),
            stop: FetchStop::Complete,
            facts: OwnedFacts::default(),
        })));
    }

    let mut builder = lane.client().get(request.url).header(RANGE, wanted.to_range_header()?);
    if let Some(etag) = request.if_match {
        builder = builder.header(IF_MATCH, etag);
    }
    if let Some(referrer) = request.referrer {
        builder = builder.header(REFERER, referrer);
    }
    if let Some(cookies) = request.cookies {
        builder = builder.header(COOKIE, cookies);
    }

    let response = builder.send().await.map_err(FetchError::Request)?;
    let facts = OwnedFacts::from_response(&response);
    let outcome = classify(wanted, &facts.as_facts());

    // The delivered range comes from Content-Range, never from what we asked
    // for: a short range is legal and its residue must be requeued.
    let delivered = match outcome {
        ProbeOutcome::Ranged { got, .. } => got,
        other => return Ok(FetchOutcome::Refused { outcome: other, facts: Box::new(facts) }),
    };

    let soft_end_at_start = claim.end_soft();
    let mut stream = response.bytes_stream();
    let mut pending = BytesMut::with_capacity(config.chunk_target);
    let mut offset = delivered.start();
    let mut last_flush = Instant::now();

    loop {
        match stream.next().await {
            Some(Ok(bytes)) => {
                if bytes.is_empty() {
                    continue;
                }
                pending.extend_from_slice(&bytes);

                let full = pending.len() >= config.chunk_target;
                let stale = last_flush.elapsed() >= config.partial_flush_after;
                if full || stale {
                    offset = flush(&mut pending, offset, request, claim, writer, budget).await?;
                    last_flush = Instant::now();
                }

                // Re-read the soft end: a steal may have landed. Dropping the
                // stream here cancels it, cheaply on H2/H3.
                if claim.is_done() {
                    break;
                }
            }
            Some(Err(source)) => {
                // Keep what did arrive -- those bytes are valid and paid for --
                // then report the failure so the residue can be requeued.
                let written = offset.saturating_sub(delivered.start());
                let _ = flush(&mut pending, offset, request, claim, writer, budget).await;
                return Err(FetchError::Body { written, source });
            }
            None => break,
        }
    }

    offset = flush(&mut pending, offset, request, claim, writer, budget).await?;

    let stop = if claim.end_soft() < soft_end_at_start {
        FetchStop::StolenFrom
    } else if claim.is_done() {
        FetchStop::Complete
    } else {
        FetchStop::ServerEnded
    };

    Ok(FetchOutcome::Fetched(Box::new(Fetched {
        written: ByteRange::try_new(delivered.start(), offset)?,
        stop,
        facts,
    })))
}

/// Hands the accumulated bytes to the writer, taking a budget permit first.
async fn flush(
    pending: &mut BytesMut,
    offset: u64,
    request: &SegmentRequest<'_>,
    claim: &LiveClaim,
    writer: &WriterHandle,
    budget: &AsyncBudget,
) -> Result<u64, FetchError> {
    if pending.is_empty() {
        return Ok(offset);
    }
    let bytes = pending.split().freeze();
    let len = bytes.len() as u64;

    // Before queueing, not after: the budget has to reflect bytes still owed
    // to the disk, and awaiting here is what closes the TCP window.
    let permit = budget.acquire(len).await;

    let job = WriteJob::new(request.generation, offset, bytes).with_permit(permit);
    writer.write(job).map_err(FetchError::Write)?;
    claim.advance(len);
    Ok(offset + len)
}
