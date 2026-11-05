# Benchmark results

Produced by `tests/bench/bench.mjs` against `tests/bench/bench-server.mjs`.
Every run verifies a SHA-256 computed independently of any downloader, so a
tool that finished first with the wrong bytes is reported as WRONG rather
than as fast. None did, in any run here.

Re-run them with:

```bash
node tests/bench/bench.mjs --size 134217728 --runs 3 --json tests/bench/results/per-connection.json
node tests/bench/bench.mjs --mode total --size 134217728 --runs 3 --json tests/bench/results/per-ip.json
node tests/bench/bench.mjs --size 134217728 --runs 3 --bps 10000000000 --rtt 0 --json tests/bench/results/unshaped.json
```

## Where the numbers came from

| | |
|---|---|
| Measured | 6 November 2026 |
| Machine | Intel Core Ultra 9 275HX, 31 GB RAM, YMTC YMSS2ED08D25MC NVMe |
| OS | Windows 11 Home Single Language 10.0.26200 |
| File | 128 MiB, deterministic bytes, three runs per tool, median reported |
| DivinePulse | `target/release/idm.exe`, rustc 1.99.1 |
| curl | 8.21.0 (Windows, Schannel) |
| aria2c | 1.37.0 |
| Node | v24.20.0 |

The disk was at 100% capacity when these were taken, which is why the
ceiling measurement needed fixing twice — see below.

## Does a number measure the scheduler or the disk?

That question is the reason the harness measures the file system first,
before any downloading, by writing the same 128 MiB to the same folder with
no network involved. A row at or near that rate is reporting the hardware;
a row well under it is reporting how many connections the tool opened and
how it spread the work, which is the thing worth comparing.

Getting that instrument right took two corrections, both worth recording
because both produced confident, wrong labels first:

- **It flushed, and the downloaders do not.** The first version called
  `fsync`, and curl then "beat the disk" by three times. Of course it did:
  none of these tools flush, so they are all racing the page cache while the
  ceiling was racing the platter. The ceiling is measured unflushed now, so
  it is really *how fast bytes can be handed to the file system* — which is
  the honest upper bound for a writer that does not flush.
- **One pass was not a measurement.** On a disk this full, a single pass
  varied by six times between runs. It is the median of three now, and the
  spread is printed beside it; when the spread is wider than two-fold the
  harness prints "unclear" in the column rather than a confident word.

**In all three runs below, every row came out bound by the scheduler**, at
roughly 4.6 GB/s of file-system headroom against a best case of 750 MB/s.
Nothing here is disk-bound. That is a property of this machine, not a
general result: on a slow spinning disk the shaped rows would be unchanged
and the unshaped ones would collapse to the disk's rate.

## 1. Per-connection shaped — `per-connection.json`

Each response capped independently at 8 MB/s, 30 ms round trip. This is the
case parallel downloading exists for, and what most CDN edges actually do.

| Tool | Median | Throughput | vs curl | Requests | Peak conns |
|---|---|---|---|---|---|
| DivinePulse (16 conn) | 1.89s | 71.0 MB/s | 13.95× | 17 | 16 |
| **DivinePulse (adaptive)** | **1.91s** | **70.2 MB/s** | **13.78×** | **17** | **16** |
| aria2c (-x16) | 2.04s | 65.8 MB/s | 12.93× | 16 | 16 |
| DivinePulse (1 conn) | 26.89s | 5.0 MB/s | 0.98× | 10 | 2 |
| curl (1 conn) | 26.36s | 5.1 MB/s | 1.00× | 1 | 1 |

DivinePulse's adaptive mode is 6.7% ahead of aria2c at sixteen fixed
connections, and lands within 1% of its own pinned-16 figure without being
told a number. That last part is the point of the adaptive controller: it
found the right width on its own.

## 2. Per-IP shaped — `per-ip.json`

One shared 8 MB/s bucket for every connection, 30 ms round trip, as a CDN
that shapes per client does. Extra connections cannot buy throughput here;
they can only cost requests.

| Tool | Median | Throughput | vs curl | Requests | Peak conns |
|---|---|---|---|---|---|
| aria2c (-x16) | 16.80s | 8.0 MB/s | 1.58× | 54 | 17 |
| DivinePulse (16 conn) | 17.46s | 7.7 MB/s | 1.52× | 19 | 16 |
| **DivinePulse (adaptive)** | **17.75s** | **7.6 MB/s** | **1.49×** | **19** | **16** |
| curl (1 conn) | 26.46s | 5.1 MB/s | 1.00× | 1 | 1 |
| DivinePulse (1 conn) | 26.88s | 5.0 MB/s | 0.98× | 10 | 2 |

**aria2c wins this one**, by 5%. It is worth saying plainly rather than
burying: on a per-IP-shaped origin aria2c extracted more throughput than
DivinePulse did.

It also made **54 requests to aria2c's 19 for DivinePulse** — nearly three
times as many to fetch the same file. That is the trade, and which side of
it is right depends on the server: a few percent of throughput against
nearly tripling the request count, on exactly the kind of origin that
answers too many requests with a 429 and a cooling-off period. This harness
cannot measure a rate limiter it does not have, so the 5% is a measured
number and the cost of 54 requests is an argument. Both are stated.

## 3. Unshaped loopback — `unshaped.json`

No bandwidth cap, no latency. Included because it is the case where
parallel downloading is pure overhead, and a benchmark that only showed the
flattering cases would not be worth keeping.

| Tool | Median | Throughput | vs curl | Requests | Peak conns |
|---|---|---|---|---|---|
| **curl (1 conn)** | **0.18s** | **750.1 MB/s** | **1.00×** | **1** | **1** |
| DivinePulse (16 conn) | 0.37s | 366.4 MB/s | 0.49× | 17 | 16 |
| DivinePulse (adaptive) | 0.38s | 352.2 MB/s | 0.47× | 17 | 13 |
| aria2c (-x16) | 0.48s | 279.6 MB/s | 0.37× | 28 | 17 |
| DivinePulse (1 conn) | 0.52s | 259.2 MB/s | 0.35× | 5 | 2 |

**curl is twice as fast as DivinePulse and four times as fast as aria2c.**
With no latency to hide and no per-connection cap to escape, one stream
with no coordination wins, and every parallel downloader pays for its
probe, its segmenting and its journal with nothing to show for it.

This is not a flaw that wants fixing so much as a boundary worth knowing.
It describes fetching from something on the same machine or the same
switch. It is also why DivinePulse's single-connection row is slower than
curl's: the extra 0.34s is the resume journal and the work ledger, which is
what buys resumability.

## Tools compared on features only

### Internet Download Manager

Deliberately not installed. It is paid and closed, and `IDMan.exe` hands
the job to a background process and returns immediately — so timing it
means watching a file grow, with a licence dialog as a failure mode, and
the number would not be comparable with tools whose process exits when the
file is done.

| | DivinePulse | IDM 6.x |
|---|---|---|
| Parallel segmented download | yes | yes |
| Adaptive connection count | yes | no — a fixed number per server |
| Resume after a crash | yes, journal + work ledger | yes |
| Browser integration | extension, opt-in cookies | extension, takes cookies by default |
| Mirrors for one file | yes, each vetted against the first | no |
| Torrents | yes | no |
| Site grabber | yes, robots.txt honoured | yes, robots.txt not honoured |
| Virus check on completion | yes, never deletes | yes, can delete |
| Scheduler | yes | yes |
| Post-download hook | program + argv, no shell | command line, shell |
| Cost | free | paid, per-licence |
| Source | open | closed |

### Ghost Downloader 3

Wired into the harness and measured the moment `ghd` is on PATH. Not
installed here because `ghd` is not a download: it is installed from inside
the application, through Settings → Command Line Tool. That means
installing a GUI program and clicking through its settings, which is a
larger thing to do to somebody's machine than unzipping a portable binary.
Say the word and it goes in.

| | DivinePulse | Ghost Downloader 3 |
|---|---|---|
| Parallel segmented download | yes | yes |
| Platforms | Windows | Windows, macOS, Linux, Android |
| CLI | yes, first-class | yes, installed from the GUI |
| RPC interface | no | yes, aria2-compatible |
| Browser integration | own extension + native host | via aria2-compatible RPC |
| Torrents | yes | yes |
| Licence | open | open (GPL-3.0) |

## What these numbers do not say

- **Nothing about the internet.** Every run is loopback against a shaped
  server. That is deliberate — a benchmark over a real link measures the
  link, and cannot be repeated — but it means the absolute rates are not
  what anyone will see.
- **Nothing about rate limiting.** The server here answers every request.
  The request counts above are the only evidence about behaviour on an
  origin that pushes back, and they are evidence, not a measurement.
- **Nothing about long downloads.** 128 MiB finishes in two seconds in the
  shaped cases. Connection churn, stalls and recovery over an hour are not
  exercised.
- **One machine, one disk, one afternoon.** Three runs per tool, median
  reported, on a machine whose disk was full.
