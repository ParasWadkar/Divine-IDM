# Benchmark results

Produced by `tests/bench/bench.mjs` against `tests/bench/bench-server.mjs`.
Every run verifies a SHA-256 computed independently of any downloader, so a
tool that finished first with the wrong bytes is reported as WRONG rather
than as fast. None did, in any run here.

Re-run them with:

```bash
export GHD_EXE="D:\CyberPunk\Ghost-Downloader-v4.4.5-Windows-x86_64\bin\ghd.exe"
node tests/bench/bench.mjs --size 134217728 --runs 5 --json tests/bench/results/per-connection.json
node tests/bench/bench.mjs --mode total --size 134217728 --runs 5 --json tests/bench/results/per-ip.json
node tests/bench/bench.mjs --size 134217728 --runs 5 --bps 10000000000 --rtt 0 --json tests/bench/results/unshaped.json
```

## Where the numbers came from

| | |
|---|---|
| Measured | 6 November 2026 |
| Machine | Intel Core Ultra 9 275HX, 31 GB RAM, YMTC YMSS2ED08D25MC NVMe |
| OS | Windows 11 Home Single Language 10.0.26200 |
| File | 128 MiB, deterministic bytes, five runs per tool, median reported |
| DivinePulse | `target/release/idm.exe`, rustc 1.99.1 |
| curl | 8.21.0 (Windows, Schannel) |
| aria2c | 1.37.0 |
| Ghost Downloader | 4.4.5, portable build, driven through its `ghd` CLI |
| Node | v24.20.0 |

Each invocation gets its own throwaway `IDM_STATE_DIR`, so our rows read
and write no real download history and no learned host profile. That was
not true until it was noticed here; see the findings.

The file-system ceiling read 2.9--4.5 GB/s across these runs, and about
4.3--4.7 GB/s in an earlier session on the same machine. Absolute figures
from different sessions are therefore **not comparable**, which is why the
comparisons below carry a control tool measured in the same sitting rather
than a remembered number.

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
2.9 GB/s or more of file-system headroom against a best case of 426 MB/s.
Nothing here is disk-bound. That is a property of this machine, not a
general result: on a slow spinning disk the shaped rows would be unchanged
and the unshaped ones would collapse to the disk's rate.

## 1. Per-connection shaped — `per-connection.json`

Each response capped independently at 8 MB/s, 30 ms round trip. This is the
case parallel downloading exists for, and what most CDN edges actually do.

| Tool | Median | Throughput | vs curl | Requests |
|---|---|---|---|---|
| **DivinePulse (16 conn)** | **1.92s** | **70.1 MB/s** | **13.73x** | 17 |
| DivinePulse (adaptive) | 1.92s | 69.9 MB/s | 13.69x | 17 |
| aria2c (-x16) | 2.10s | 64.0 MB/s | 12.54x | 17 |
| Ghost Downloader (ghd) | 3.91s | 34.3 MB/s | 6.72x | 10 |
| curl (1 conn) | 26.27s | 5.1 MB/s | 1.00x | 1 |
| DivinePulse (1 conn) | 26.78s | 5.0 MB/s | 0.98x | 10 |

DivinePulse's adaptive mode is 9% ahead of aria2c at sixteen fixed
connections and level with its own pinned sixteen, which is the point of the
controller: it found the right width without being told a number.

Ghost Downloader is half the speed of the other two parallel downloaders,
and the request count says why: it opened **9 connections where the others
opened 16**, so against an origin that caps each connection it left about
half the available throughput unclaimed. That is its default, not its
ceiling -- it was run exactly as it comes.

## 2. Per-IP shaped — `per-ip.json`

One shared 8 MB/s bucket for every connection, 30 ms round trip, as a CDN
that shapes per client does. Extra connections cannot buy throughput here;
they can only cost requests.

| Tool | Median | Throughput | vs curl | Requests |
|---|---|---|---|---|
| **Ghost Downloader (ghd)** | **16.77s** | **8.0 MB/s** | **1.57x** | 50 |
| aria2c (-x16) | 16.78s | 8.0 MB/s | 1.57x | 56 |
| DivinePulse (adaptive) | 17.27s | 7.8 MB/s | 1.52x | 33 |
| DivinePulse (16 conn) | 17.46s | 7.7 MB/s | 1.50x | 19 |
| curl (1 conn) | 26.27s | 5.1 MB/s | 1.00x | 1 |
| DivinePulse (1 conn) | 26.80s | 5.0 MB/s | 0.98x | 10 |

**DivinePulse is third here, about 2.8% behind both.** Worth saying plainly
rather than burying: on a per-IP-shaped origin, both of them extract more
throughput than it does. Four attempts to close it are recorded below, and
the one that worked cost more elsewhere than it gained here.

They also made **50 and 56 requests against DivinePulse's 33**, about half
as many again to fetch the same file. That is the trade, and which side of
it is right depends on the server: a few percent of throughput against
double the request count, on exactly the kind of origin that answers too
many requests with a 429 and a cooling-off period. This harness cannot
measure a rate limiter it does not have, so the 2.8% is a measured number
and the cost of 50 requests is an argument. Both are stated.

## 3. Unshaped loopback — `unshaped.json`

No bandwidth cap, no latency. Included because it is the case where
parallel downloading is pure overhead, and a benchmark that only showed the
flattering cases would not be worth keeping.

| Tool | Median | Throughput | vs curl | Requests |
|---|---|---|---|---|
| **curl (1 conn)** | **0.32s** | **425.5 MB/s** | **1.00x** | 1 |
| DivinePulse (16 conn) | 0.41s | 324.7 MB/s | 0.76x | 17 |
| DivinePulse (adaptive) | 0.43s | 312.2 MB/s | 0.73x | 17 |
| DivinePulse (1 conn) | 0.65s | 207.4 MB/s | 0.49x | 5 |
| aria2c (-x16) | 0.67s | 199.4 MB/s | 0.47x | 27 |
| Ghost Downloader (ghd) | 0.68s | 196.1 MB/s | 0.46x | 39 |

**curl is 1.3x faster than DivinePulse**, and DivinePulse is in turn 1.6x
faster than both other parallel downloaders. With no latency to hide and no
per-connection cap to escape, one stream with no coordination wins, and
every parallel downloader pays for its probe, its segmenting and its
journal with nothing to show for it. That DivinePulse pays less of it than
aria2c and ghd do is the only thing worth reading into this table.

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

### Per-IP shaping: still 2.8% behind, and now for a known reason

**Confirmed: the controller never came back down.** Running our CLI with
`--json` -- a flag that existed for this and was never used -- showed it
settling at sixteen connections. A sweep against a shared 8 MB/s bucket,
128 MiB:

| Connections | 1 | 2 | 3 | 4 | 8 | 16 |
|---|---|---|---|---|---|---|
| MB/s | 5.0 | **8.36** | 8.19 | 7.90 | 7.55 | 7.64 |

Settling only ever stopped the climb: *"Settled means settled; only
`allow_one_probe` or a throttle moves the count again."* The only path that
reduced the count was a 429. It now keeps a rate per count and lands on the
lowest one within noise of the best, which is the fact a single high-water
mark cannot express -- on a shared bucket every count from the knee upwards
reads the *same* rate, so "better" never fires and the mark stays wherever
the climb stopped.

**Refuted, four times over: finer claims.** `claim_size` asks for
`remaining / (4 * connections)` and a preferred floor of 8 MiB hands back
8 MiB, which on 128 MiB over sixteen connections is one claim each and
nothing spare. A trace shows the consequence plainly: sixteen connections
becoming nine without a single decision being taken, because a worker that
finishes finds nothing left to claim. The steal counter reads zero
throughout -- which had been taken as evidence claim sizing was fine, and
was evidence of the opposite.

So the floor came off. Four schemes were measured, 128 MiB, five runs,
median, with aria2c reading 2.08--2.10s throughout as the control:

| Claims | Per-connection | Requests | Shared bucket |
|---|---|---|---|
| **8 MiB (as shipped)** | **1.91s, 70.4 MB/s** | **17** | 17.18s, 7.8 MB/s |
| Divisor alone, 2 MiB | 2.12s, 63.2 MB/s | 97 | 16.74s, 8.0 MB/s |
| Two per connection, 4 MiB | 2.21s, 60.7 MB/s | 43 | 16.71s, 8.0 MB/s |
| Two seconds of work, constant | 2.36s, 56.9 MB/s | 28 | 16.86s, 8.0 MB/s |

Finer claims do win the shared bucket, by about 2.6%. They lose the
per-connection case by up to 19%, and they lose it to the requests
themselves: each extra claim is a round trip and a fresh slow start, and
eleven of those at sixteen connections on a 30 ms path cost more than the
better balance recovers. Trading a decisive win for a narrow one is not a
trade worth making, so the floor went back.

The best of the four is worth describing because it was the most
principled and still lost. A claim is the unit of commitment -- until a
worker finishes one it cannot be stopped, cannot hand work back and cannot
be rebalanced -- so sizing it as *two seconds of one connection's measured
work* makes it coarse on a capped origin and fine on a shared bucket with
no knowledge of which is which. It does exactly that, and the arithmetic is
right; the per-connection case simply cannot afford any extra requests at
all.

**A measurement lesson, paid for.** The first A/B compared against a figure
from an earlier session and appeared to show a 19% regression from the
claim change alone. The disk ceiling on this machine had fallen from
4334 MB/s to about 2900 between sessions, so the two numbers were not
comparable. Re-run in one sitting with aria2c as a control, the regression
was real and the same size -- but it could as easily not have been, and
nothing in the first comparison would have said so.

**Also fixed, and found the same way:** the benchmark never set
`IDM_STATE_DIR`, so every run read and wrote the real state directory under
LOCALAPPDATA -- the actual download history, and the learned host profiles
with it. A profile written by run 1 was read by run 2, so run 1 and run 5
were not the same experiment and the median quietly mixed them.

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

**Per-IP is 2.8% behind after three rounds, and the obstacle is now a
measurement artefact rather than a mechanism.** The three rounds were: give
the ramp a way back down; cut claims finer, four ways; and make a reduction
actually reach the lanes. All three are in, all three are correct, and the
median has not moved -- 17.18s, then 17.22s, then 17.27s.

What is left is that the descent lands on eight connections where the curve
peaks at two. In-flight now follows the target down through 16, 8, 4, 2, so
the count is genuinely reachable; the controller simply does not know two is
the answer. Its per-count table is poisoned by its own first readings: the
bench server's token bucket starts full, so the opening windows report
24 MB/s on an 8 MB/s link, and those get recorded against whichever count
happened to be current. Every later comparison is made against a number
that was never achievable.

That is fixable -- discard windows until the rate settles, or keep a mean of
later windows per count rather than the best one -- and it is not obviously
fixable *cheaply*, because the per-connection case decides everything in
its first two windows and finishes in 1.9 seconds. Patience is free on a
seventeen-second download and expensive on a two-second one. A real CDN
with a burst allowance behaves the same way as this server, so the artefact
is worth solving rather than engineering around.

**Stealing is the other untried lever.** `steals` reads zero in every run of
every shape here, and the gate says why: a split must save
`min_gain_secs: 2.0` **in absolute terms**, on a download lasting 1.9
seconds in the per-connection case, and the remainder must be 8 MiB with
4 MiB halves. "Don't churn for a trivial gain" should scale with the
download, and `min_gain_frac: 0.15` beside it already says it that way.
Those constants were chosen for long downloads and have never been measured
against a short one.

**Unshaped is not reachable by trimming.** We make a probe request and then
range requests; curl makes one request and cannot resume or parallelise
because of it. On a harness charging ~40ms per request that difference is
most of the gap, and it is the price of the features, not waste. Both other
parallel downloaders pay it too, and more: 0.41s against aria2c's 0.67 and
ghd's 0.68.

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

## Where Ghost Downloader is genuinely ahead

Taken from its own README and release notes in November 2026 rather than
from guesswork, and checked against this codebase one line at a time. Most
of its headline features turned out not to be gaps at all:

| ghd claims | Here |
|---|---|
| "Smart chunking without file merging" | Same thing: sparse file, each chunk written at its offset, no merge pass |
| "Auto-detects bandwidth" | The adaptive connection controller |
| HTTP, FTP, Magnet/BT | Yes |
| M3U8, MPEG-DASH | Yes, `idm-media` plus `idm-net::stream`, with HLS decryption |
| Hash verification (MD5/SHA1/SHA256) | Yes |
| "aria2-compatible RPC API" | Yes, since this round -- loopback only, which ghd's is not |
| Batch add, drag-drop, paste, per-task threads | Yes |
| SOCKS5/HTTP/HTTPS proxy, optional cert verification | Yes |
| Change the URL mid-download | Yes, "New link" |
| Runs headless | Yes; ghd's CLI starts the app in the background |

What is actually missing, in the order it would be worth doing:

1. **Four platforms against one.** Windows, macOS, Linux and Android,
   against Windows only. This is the real gap and it is not a feature, it
   is the `win/` module, WebView2, Credential Manager and the registry.
   Months, not an afternoon.
2. **Browser TLS fingerprints.** ghd presents a real browser's TLS
   handshake; this presents `DivinePulse/<version>` over a default
   handshake, which some CDNs refuse. Rust has several BoringSSL-based
   answers -- `wreq` with 60+ profiles, `rquest`, `impersonate-rs` -- but
   each replaces `reqwest` across the whole network layer and adds a
   vendored C TLS stack to the build. It is also, plainly, defeating bot
   detection, which is a decision to take deliberately rather than slide
   into: a configurable User-Agent, which already exists here, covers the
   ordinary cases.
3. **HTTP/3.** `reqwest`'s support is behind both an `http3` feature and
   `RUSTFLAGS="--cfg reqwest_unstable"`, so taking it means the build
   depends on an unstable cfg. `HostProfile::h3_won_race` was designed for
   this and is still unused.
4. **Site parsers.** YouTube and Bilibili with quality, subtitle and
   playlist selection; GitHub and HuggingFace mirror acceleration. ghd also
   just accepts yt-dlp tasks, which is the cheap version of most of this.
   The mirror lists route somebody's downloads through third-party proxies,
   which is a trust decision and not only a feature.
5. **Changing headers or the proxy mid-download.** `RefreshUrl` carries a
   replacement address and nothing else.
6. **eD2k.** A whole protocol for a network that has largely gone.

Not gaps despite appearances: ghd's plugin API is on its roadmap rather
than shipped, and its "AI acceleration" is, by its own release notes, a
feature that raises the thread count.

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
| CLI output | human text, or JSON with `--json` | JSON on stdout |
| Documented exit codes | yes, five kinds, under `--help` | yes |
| JSON on failure as well as success | yes | yes |
| Runs headless | yes | no — the CLI starts the app in the background |
| RPC interface | yes, aria2-compatible, loopback only | yes, aria2-compatible |
| RPC can be exposed to the network | no, by design | yes, `--rpc-listen-all` |
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
- **Nothing about tuned configurations.** Every tool was run as it comes,
  with the one exception of `aria2c -x16`, which needs an explicit number
  to do parallel downloading at all. Ghost Downloader's 9 connections are
  its default, not its ceiling.
- **Nothing about long downloads, and this now matters.** 128 MiB finishes
  in two seconds in the per-connection case and seventeen in the shared
  one. The adaptive descent needs claims to complete before a decision to
  use fewer connections can take effect, and at 8 MiB over a shared bucket
  a claim takes about as long as the whole download -- so the one thing it
  was built for is the thing this harness cannot show.
- **One machine, one disk.** Five runs per tool, median reported, with a
  file-system ceiling that moved by half between sessions.
