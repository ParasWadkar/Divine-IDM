//! `idm` — the command line driver.
//!
//! Exists before any GUI on purpose: the engine's throughput has to be
//! measurable against other downloaders before it is worth putting a window on
//! top of it. `--json` emits one machine-readable line for the benchmark
//! harness.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use idm_net::download::{DownloadConfig, DownloadReport, Progress, download_with_progress};

#[derive(Parser, Debug)]
#[command(name = "idm", about = "Divine IDM — a parallel, resumable downloader", version)]
struct Args {
    /// URL to download.
    url: String,

    /// Directory to write into.
    #[arg(short = 'o', long, default_value = ".")]
    out: PathBuf,

    /// Override the filename.
    #[arg(long)]
    name: Option<String>,

    /// Pin the connection count, disabling the adaptive ramp.
    ///
    /// Normally left unset: the right number depends on the bandwidth-delay
    /// product, whether the origin shapes per connection or per IP, the disk
    /// ceiling and the file size, so it is measured rather than guessed. Fixing
    /// it is for benchmarking and for servers that misbehave under tuning.
    #[arg(short = 'n', long)]
    connections: Option<u32>,

    /// Upper bound for the adaptive ramp.
    #[arg(long, default_value_t = 16)]
    max_connections: u32,

    /// Use SetFileValidData to skip NTFS zero-filling.
    ///
    /// Faster on volumes without sparse support, but it exposes previously
    /// deleted on-disk content through a readable file and forces a full
    /// verification pass on every resume. Off unless asked for.
    #[arg(long)]
    aggressive_prealloc: bool,

    /// Don't pin lanes to distinct resolved addresses.
    #[arg(long)]
    no_edge_pinning: bool,

    #[arg(long)]
    referrer: Option<String>,

    /// Cookie header to forward, for a download behind a login.
    #[arg(long)]
    cookies: Option<String>,

    /// One line of JSON on completion, for scripts.
    #[arg(long)]
    json: bool,

    /// No progress output.
    #[arg(short, long)]
    quiet: bool,
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}

fn human_rate(bytes_per_sec: f64) -> String {
    format!("{}/s", human_bytes(bytes_per_sec as u64))
}

fn render_progress(progress: &Progress) {
    let width = 28usize;
    let filled = ((progress.fraction() * width as f64).round() as usize).min(width);
    let bar: String =
        std::iter::repeat_n('#', filled).chain(std::iter::repeat_n('-', width - filled)).collect();

    let note = match progress.settled {
        // Worth distinguishing: these are very different problems for the user.
        Some(idm_core::sched::conn::SettleReason::DiskBound) => " disk-limited",
        Some(idm_core::sched::conn::SettleReason::PerIpShaping) => " server-shaped",
        _ => "",
    };

    eprint!(
        "\r[{bar}] {:>5.1}%  {:>7}  {:>10}  {:>12}  {} conn{}  ",
        progress.fraction() * 100.0,
        format!("{:.0}s", progress.elapsed.as_secs_f64()),
        human_bytes(progress.durable),
        human_rate(progress.rate_bps),
        progress.connections,
        note
    );
    let _ = std::io::stderr().flush();
}

fn print_json(report: &DownloadReport) {
    // Hand-rolled rather than pulling in serde for four fields.
    println!(
        concat!(
            "{{\"path\":\"{}\",\"total_bytes\":{},\"elapsed_secs\":{:.4},",
            "\"bytes_per_sec\":{:.1},\"connections\":{},\"lanes\":{},",
            "\"requests\":{},\"steals\":{},\"hedges\":{},\"retries\":{},",
            "\"resumed_bytes\":{},\"write_mode\":\"{:?}\",\"settled\":\"{:?}\"}}"
        ),
        report.path.display().to_string().replace('\\', "\\\\"),
        report.total,
        report.elapsed.as_secs_f64(),
        report.mean_bytes_per_sec(),
        report.connections_settled_at,
        report.lanes,
        report.stats.requests,
        report.stats.steals,
        report.stats.hedges,
        report.stats.retries,
        report.resumed_bytes,
        report.write_mode,
        report.settle_reason
    );
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let args = Args::parse();
    tracing_subscriber_init();

    let mut config = DownloadConfig::new(&args.url, &args.out);
    config.filename = args.name.clone();
    config.fixed_connections = args.connections;
    config.conn.per_host_max = args.max_connections.max(1);
    config.allow_set_valid_data = args.aggressive_prealloc;
    config.pin_edges = !args.no_edge_pinning;
    config.referrer = args.referrer.clone();
    config.cookies = args.cookies.clone();

    let show_progress = !args.quiet && !args.json && std::io::stderr().is_terminal();
    let mut last_render = std::time::Instant::now();

    let outcome = download_with_progress(&config, |progress| {
        // Throttled so a fast download does not spend its time drawing.
        if show_progress && last_render.elapsed() >= Duration::from_millis(100) {
            render_progress(&progress);
            last_render = std::time::Instant::now();
        }
    })
    .await;

    match outcome {
        Ok(report) => {
            if show_progress {
                eprintln!();
            }
            if args.json {
                print_json(&report);
            } else if !args.quiet {
                eprintln!(
                    "{} — {} in {:.2}s ({}, {} connections, {} lanes)",
                    report.path.display(),
                    human_bytes(report.total),
                    report.elapsed.as_secs_f64(),
                    human_rate(report.mean_bytes_per_sec()),
                    report.connections_settled_at,
                    report.lanes,
                );
                if report.stats.steals > 0 || report.stats.hedges > 0 {
                    eprintln!(
                        "  {} requests, {} steals, {} hedges, {} retries",
                        report.stats.requests,
                        report.stats.steals,
                        report.stats.hedges,
                        report.stats.retries
                    );
                }
                if report.resumed_bytes > 0 {
                    eprintln!("  resumed from {}", human_bytes(report.resumed_bytes));
                }
            }
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            if show_progress {
                eprintln!();
            }
            eprintln!("error: {error}");
            // The partial file and journal are deliberately left in place so a
            // later run can resume rather than start over.
            std::process::ExitCode::FAILURE
        }
    }
}

fn tracing_subscriber_init() {
    // Logging stays off unless asked for, so the progress line is not fought
    // over. RUST_LOG is honoured by whatever subscriber a host installs.
    if std::env::var_os("RUST_LOG").is_some() {
        tracing::debug!("tracing enabled via RUST_LOG");
    }
}
