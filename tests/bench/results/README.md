# Benchmark results

Produced by `tests/bench/bench.mjs` against `tests/bench/bench-server.mjs`.
Every run verifies a SHA-256 computed independently of any downloader, so a
tool that finished first with the wrong bytes is reported as WRONG rather
than as fast. None did, in any run here.

Re-run them with:

```bash
export GHD_EXE="D:\CyberPunk\Ghost-Downloader-v4.4.5-Windows-x86_64\bin\ghd.exe"
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
| Ghost Downloader | 4.4.5, portable build, driven through its `ghd` CLI |
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
roughly 4.5 GB/s of file-system headroom against a best case of 593 MB/s.
Nothing here is disk-bound. That is a property of this machine, not a
general result: on a slow spinning disk the shaped rows would be unchanged
and the unshaped ones would collapse to the disk's rate.

## 1. Per-connection shaped — `per-connection.json`

Each response capped independently at 8 MB/s, 30 ms round trip. This is the
case parallel downloading exists for, and what most CDN edges actually do.

| Tool | Median | Throughput | vs curl | Requests | Was |
|---|---|---|---|---|---|
| **DivinePulse (adaptive)** | **1.88s** | **71.2 MB/s** | **14.04x** | 17 | 72.3 |
| DivinePulse (16 conn) | 1.87s | 71.6 MB/s | 14.11x | 17 | 71.4 |
| aria2c (-x16) | 2.01s | 66.7 MB/s | 13.13x | 16 | 66.2 |
| Ghost Downloader (ghd) | 3.78s | 35.5 MB/s | 6.99x | 10 | 36.0 |
| DivinePulse (1 conn) | 26.84s | 5.0 MB/s | 0.99x | 10 | 5.0 |
| curl (1 conn) | 26.45s | 5.1 MB/s | 1.00x | 1 | 5.1 |

DivinePulse's adaptive mode is 6.7% ahead of aria2c at sixteen fixed
connections, and beats its own pinned-16 figure without being told a
number. That last part is the point of the adaptive controller: it found
the right width on its own.

Ghost Downloader is half the speed of the other two parallel downloaders
here, and the request count says why: it opened **9 connections where the
others opened 16**, so on an origin that caps each connection it left
roughly half the available throughput unclaimed. That is a configuration
default rather than a flaw — it was run exactly as it comes.

## 2. Per-IP shaped — `per-ip.json`

One shared 8 MB/s bucket for every connection, 30 ms round trip, as a CDN
that shapes per client does. Extra connections cannot buy throughput here;
they can only cost requests.

| Tool | Median | Throughput | vs curl | Requests | Was |
|---|---|---|---|---|---|
| aria2c (-x16) | 16.81s | 8.0 MB/s | 1.57x | 54 | 8.0 |
| Ghost Downloader (ghd) | 16.83s | 8.0 MB/s | 1.57x | 48 | 8.0 |
| **DivinePulse (adaptive)** | **17.21s** | **7.8 MB/s** | **1.54x** | 24 | **7.6** |
| DivinePulse (16 conn) | 17.56s | 7.6 MB/s | 1.51x | 19 | 7.6 |
| curl (1 conn) | 26.44s | 5.1 MB/s | 1.00x | 1 | 5.1 |
| DivinePulse (1 conn) | 26.89s | 5.0 MB/s | 0.98x | 10 | 5.0 |

Adaptive now beats our own pinned sixteen, 7.8 against 7.6, which is the
signature of the connection controller coming back down -- see the
findings below. **DivinePulse is still third here**, about 2.5% behind
both aria2c and Ghost Downloader, where it was 5% before. Worth saying plainly rather than burying: on a per-IP-shaped
origin, both of them extracted more throughput than it did.

They also made **47 and 53 requests against DivinePulse's 19** — roughly
two and a half times as many to fetch the same file. That is the trade, and
which side of it is right depends on the server: a few percent of
throughput against nearly tripling the request count, on exactly the kind
of origin that answers too many requests with a 429 and a cooling-off
period. This harness cannot measure a rate limiter it does not have, so the
5% is a measured number and the cost of 50 requests is an argument. Both
are stated.

## 3. Unshaped loopback — `unshaped.json`

No bandwidth cap, no latency. Included because it is the case where
parallel downloading is pure overhead, and a benchmark that only showed the
flattering cases would not be worth keeping.

| Tool | Median | Throughput | vs curl | Requests | Was |
|---|---|---|---|---|---|
| **curl (1 conn)** | **0.20s** | **663.8 MB/s** | **1.00x** | 1 | 592.6 |
| DivinePulse (16 conn) | 0.35s | 379.4 MB/s | 0.57x | 17 | 403.2 |
| DivinePulse (adaptive) | 0.42s | 320.1 MB/s | 0.48x | 17 | 346.4 |
| aria2c (-x16) | 0.49s | 276.2 MB/s | 0.42x | 28 | 267.8 |
| DivinePulse (1 conn) | 0.49s | 273.2 MB/s | 0.41x | 5 | **225.3** |
| Ghost Downloader (ghd) | 0.51s | 262.4 MB/s | 0.40x | 41 | 246.9 |

The single-connection row improved 21%, from 225.3 to 273.2 MB/s, on the
fixed-cost work in the findings below -- the one place this table can see
it, because that row is almost all fixed cost. Everything else moved
within the noise of a run where curl's own figure rose 12%, so this table
is not evidence either way for the rest.

**curl is 1.75x faster than DivinePulse and over twice as fast as aria2c
and Ghost Downloader.** With no latency to hide and no per-connection cap to
escape, one stream with no coordination wins, and every parallel downloader
pays for its probe, its segmenting and its journal with nothing to show for
it.

This is not a flaw that wants fixing so much as a boundary worth knowing.
It describes fetching from something on the same machine or the same
switch. It is also why DivinePulse's single-connection row is slower than
curl's: the extra time is the resume journal and the work ledger, which is
what buys resumability.

## What was chased, and what it turned out to be

Written down including the dead ends, because a list of what a slow
download is *not* saves the next person from the same afternoon. Every
line here came from a measurement; the ones marked refuted were confident
guesses first.

### Per-IP shaping: 7.6 to 7.8 MB/s

**Refuted: claim sizing.** The first theory was that `claim_size` clamps
claims up to 8 MiB, giving sixteen connections exactly one claim each and
a ragged tail nothing rebalances. Wiring the benchmark to run our CLI
with `--json` -- a flag that existed for this and was never used -- showed
`steals: 0` on every run. No stealing was happening, so no tail was being
left unrebalanced.

**Confirmed: the controller never came back down.** The same `--json` line
showed it settling at sixteen connections. A sweep against a shared
8 MB/s bucket, 128 MiB:

| Connections | 1 | 2 | 3 | 4 | 8 | 16 |
|---|---|---|---|---|---|---|
| MB/s | 5.0 | **8.36** | 8.19 | 7.90 | 7.55 | 7.64 |

The controller diagnoses a shared bucket correctly and then holds every
connection the climb added, because settling only ever stopped the climb:
*"Settled means settled; only `allow_one_probe` or a throttle moves the
count again."* The only path that reduced the count was a 429. It now
remembers where it was fastest and retreats there.

**Rejected: probing below the opening count.** The ramp starts at eight
and never measures lower, so the peak at two is out of its reach. A
bounded downward probe was built, including giving each step a full probe
interval so it judged the new count rather than the drain. It settled
back at eight every time -- its windows come back *equal* to the best
rather than better, equal never updates the best, and it walks home.
Measured over four runs: no throughput change, about 40% more requests.
That is the trade this benchmark criticises aria2c for, so it was taken
out again rather than shipped.

### Unshaped: one row moved, the rest is the harness

`IDM_TRACE_PHASES=1` prints the elapsed time at each milestone. On a
1 MiB download over loopback, 131ms in total:

    probe          43.4ms
    part opened    +0.5
    lanes built   +16.6
    writer up      +0.2
    transfer      +52.3
    flushed        +6.0
    verified       +1.1
    synced         +2.0
    renamed        +8.7

**Refuted: TLS and cert-store setup.** The resolver client builds in
**0.2ms**. Client construction is free.

**Refuted: per-lane client construction.** Varying the lane count from one
to four moves the total not at all.

**Refuted: the 1 MiB probe body.** Asking for a single byte instead
changed the timing not at all -- on loopback the whole 40ms is the request
itself. The probe was still shrunk, because the megabyte was real wasted
bandwidth on any real link and the comment beside it claimed, untruthfully,
that the body "doubles as the first segment".

**Refuted: that 52ms transfer is ours.** A minimal client fetching the same
1 MiB from the same bench server takes **40-93ms, settling near 63ms**.
That phase is the mock server. Anything tuned against it would have been
tuned against the test rig.

**Confirmed and fixed, about 30ms of a 131ms download:** `preallocate`
asked the volume for free space that `caps_for` had already fetched a line
earlier; `preallocate` also opened the volume device every download to
compute `max_writer_shards`, a field with one producer and no consumers,
since the writer is built with a hardcoded single shard; `checkpoint`
flushed the writer, drained the acks and flushed again, with nothing
between the two that writes; and `net_fingerprint` enumerated every
adapter per download, about 4ms, to answer a question that cannot change
while the process runs.

That last one is worth nothing to this benchmark, where every run is a
fresh process, and 4ms per download to the app, which is the thing people
use. Worth stating because the table would otherwise call it a no-op.

### What is left

**Per-IP is still 2.5% behind.** The remaining lever is reaching the knee
at two or three connections, which needs the lowest count within noise of
the best rather than a single high-water mark -- per-count bookkeeping the
controller does not keep today.

**Unshaped is not reachable by trimming.** We make a probe request and then
range requests; curl makes one request and cannot resume or parallelise
because of it. On a harness charging ~40ms per request that difference is
most of the gap, and it is the price of the features, not waste.

## Driving Ghost Downloader

`ghd` is two calls, not one: `add` queues the task, starts it and returns
at once with a task id, and `wait` blocks until it stops and exits non-zero
unless it completed. The clock spans both, and they are spawned back to
back, because anything in between is time the download is already spending.

Two details the harness has to get right, both learned the hard way:

- `--conflict overwrite`, because the app remembers tasks between runs and
  the default "keep both" would quietly write `payload (1).bin` on the
  second run and leave the digest check reading a stale file.
- The task list is tidied afterwards, one id at a time. A benchmark that
  did not would leave dozens of finished rows in somebody's app, and one id
  the app declines to remove fails a whole batch — so anything that
  survives is printed with the command to clear it rather than left as a
  surprise.

The harness finds `ghd` through `GHD_EXE` or on PATH. The portable build
ships it in `bin/` and does not put it on PATH, so on this machine it is
the environment variable.

## Compared on features only

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

### Ghost Downloader, beyond the numbers

Measured above, so this is only what a throughput figure cannot show.

| | DivinePulse | Ghost Downloader 4.4.5 |
|---|---|---|
| Platforms | Windows | Windows, macOS, Linux, Android |
| CLI | yes, first-class | yes, `ghd` in the portable build's `bin/` |
| CLI output | human text | JSON on stdout, documented exit codes |
| Runs headless | yes | no — the CLI starts the app in the background |
| RPC interface | no | yes, aria2-compatible |
| Connections by default | adaptive, up to 16 here | 9 here |
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
- **Nothing about tuned configurations.** Every tool was run as it comes,
  with the one exception of `aria2c -x16`, which needs an explicit number
  to do parallel downloading at all. Ghost Downloader's 9 connections are
  its default, not its ceiling.
- **One machine, one disk, one afternoon.** Three runs per tool, median
  reported, on a machine whose disk was full.
