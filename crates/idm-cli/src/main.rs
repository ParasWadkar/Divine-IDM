//! `idm` — the command line driver.
//!
//! Exists before any GUI on purpose: the engine's throughput has to be
//! measurable against other downloaders before it is worth putting a window on
//! top of it. `--json` emits one machine-readable line for the benchmark
//! harness.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::Duration;

mod install;

use clap::Parser;
use idm_core::sched::ProfileStore;
use idm_net::download::{DownloadConfig, DownloadReport, Progress, download_with_progress};

#[derive(Parser, Debug)]
#[command(name = "idm", about = "Divine IDM — a parallel, resumable downloader", version)]
struct Args {
    /// URL to download. Omitted when running with --serve.
    url: Option<String>,

    /// Run as the local endpoint the browser extension talks to, instead of
    /// downloading one URL and exiting.
    #[arg(long)]
    serve: bool,

    /// Register the native messaging host with every browser found.
    #[arg(long)]
    install_host: bool,

    /// Remove the native messaging host registration from every browser.
    #[arg(long)]
    uninstall_host: bool,

    /// Chromium extension id permitted to launch the host. Repeatable.
    ///
    /// Required for Chromium browsers: the id is what the browser checks
    /// before it will start the host at all, so an empty list would mean any
    /// extension could.
    #[arg(long = "extension-id")]
    extension_ids: Vec<String>,

    /// Gecko extension id permitted to launch the host. Repeatable.
    #[arg(long = "gecko-id")]
    gecko_ids: Vec<String>,

    /// Register with every known browser, not only the installed ones, so a
    /// browser installed later still finds the host.
    #[arg(long)]
    all_browsers: bool,

    /// Show pairing requests and paired extensions.
    #[arg(long)]
    pairings: bool,

    /// Approve a pending pairing request by its id.
    #[arg(long)]
    approve: Option<String>,

    /// Deny a pending pairing request by its id.
    #[arg(long)]
    deny: Option<String>,

    /// Revoke a paired extension by its client id.
    #[arg(long)]
    revoke: Option<String>,

    /// Turn browser integration on or off, or show it with no value.
    ///
    /// Off makes the engine refuse anything a browser hands it, so downloads
    /// stay in the browser. It is enforced here rather than in the extension,
    /// so it holds even if an extension is compromised or out of date -- and
    /// it saves hunting through each browser's own settings to disable them
    /// one at a time.
    #[arg(long, value_name = "on|off", num_args = 0..=1)]
    integration: Option<Option<IntegrationArg>>,

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

    /// Override the HTTP/2 initial stream window, in bytes.
    ///
    /// hyper defaults to 64 KiB, which caps one stream at window/RTT --
    /// about 3 MB/s at a 20 ms round trip. Exposed so the effect can be
    /// measured rather than asserted.
    #[arg(long)]
    h2_window: Option<u32>,

    /// Disable HTTP/2 adaptive window sizing.
    #[arg(long)]
    no_h2_adaptive: bool,

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

    /// Username, for a server that wants one.
    ///
    /// Preferred over putting credentials in the address: a URL reaches the
    /// job list, the history and any log line that prints it, and a password
    /// in one ends up in all three.
    #[arg(long)]
    user: Option<String>,

    /// Password, for a server that wants one.
    #[arg(long)]
    password: Option<String>,

    /// For a stream, take the best rendition no taller than this.
    ///
    /// Streams offer several renditions of the same video; without this the
    /// largest is taken, which is usually what someone downloading it wants.
    #[arg(long, value_name = "PIXELS")]
    max_height: Option<u64>,

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
            "\"resumed_bytes\":{},\"write_mode\":\"{:?}\",\"settled\":\"{:?}\",",
            "\"http_version\":\"{}\"}}"
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
        report.settle_reason,
        report.http_version
    );
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let args = Args::parse();
    tracing_subscriber_init();

    if args.install_host {
        return run_install(&args);
    }
    if args.uninstall_host {
        let report = install::uninstall();
        for entry in &report.registered {
            eprintln!("  removed from {}", entry.label);
        }
        if report.registered.is_empty() {
            eprintln!("nothing was registered");
        }
        return std::process::ExitCode::SUCCESS;
    }
    if let Some(choice) = args.integration {
        return run_integration_command(choice).await;
    }
    if args.pairings || args.approve.is_some() || args.deny.is_some() || args.revoke.is_some() {
        return run_pairing_command(&args).await;
    }

    if args.serve {
        return match idm_engine::serve::run(args.out.clone(), args.max_connections.max(1)).await {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("error: {error}");
                std::process::ExitCode::FAILURE
            }
        };
    }

    let Some(url) = args.url.clone() else {
        eprintln!("error: a URL is required unless --serve is given");
        return std::process::ExitCode::FAILURE;
    };

    // A download works without the store; it only starts better informed,
    // so a store that will not open is a warning rather than a failure.
    let store = idm_engine::open_store();

    let mut config = DownloadConfig::new(&url, &args.out);
    config.profiles = store.clone().map(|store| store as std::sync::Arc<dyn ProfileStore>);
    config.filename = args.name.clone();
    config.fixed_connections = args.connections;
    config.conn.per_host_max = args.max_connections.max(1);
    config.allow_set_valid_data = args.aggressive_prealloc;
    config.http.h2_adaptive_window = !args.no_h2_adaptive;
    if let Some(window) = args.h2_window {
        config.http.h2_stream_window = window;
        config.http.h2_connection_window = window.saturating_mul(4).max(window);
    }
    config.pin_edges = !args.no_edge_pinning;
    config.referrer = args.referrer.clone();
    config.cookies = args.cookies.clone();

    let show_progress = !args.quiet && !args.json && std::io::stderr().is_terminal();
    let mut last_render = std::time::Instant::now();

    // A manifest is not a file. Recognised by its address here; the body is
    // what actually decides, inside.
    if idm_net::stream::looks_like_manifest(&url) {
        return download_stream(&url, &args, show_progress).await;
    }
    if idm_net::ftp::handles(&url) {
        return download_ftp(&url, &args, show_progress).await;
    }

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

/// Registers the native messaging host with whatever browsers are installed.
fn run_install(args: &Args) -> std::process::ExitCode {
    // The host sits beside this binary, which is how it is shipped.
    let host_path = match std::env::current_exe() {
        Ok(exe) => exe.with_file_name("idm-host.exe"),
        Err(error) => {
            eprintln!("error: cannot locate this executable: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let defaults = install::AllowedCallers::default();
    let callers = install::AllowedCallers {
        chromium_ids: args.extension_ids.clone(),
        gecko_ids: if args.gecko_ids.is_empty() {
            defaults.gecko_ids
        } else {
            args.gecko_ids.clone()
        },
    };
    if callers.chromium_ids.is_empty() {
        eprintln!(
            "error: at least one --extension-id is required for Chromium browsers.\n\
             Load the extension unpacked, copy its ID from the browser's\n\
             extensions page, and pass it here."
        );
        return std::process::ExitCode::FAILURE;
    }

    match install::install(&host_path, &callers, args.all_browsers, &[]) {
        Ok(report) => {
            for entry in &report.registered {
                let note = if entry.detected { "installed" } else { "pre-registered" };
                eprintln!("  registered with {} ({note})", entry.label);
            }
            if !report.skipped.is_empty() && !args.all_browsers {
                eprintln!(
                    "  skipped {} browser(s) that are not installed; use --all-browsers to register anyway",
                    report.skipped.len()
                );
            }
            for (label, error) in &report.failures {
                eprintln!("  FAILED for {label}: {error}");
            }
            eprintln!("host: {}", host_path.display());
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Pairing management, which talks to a running engine.
/// What `--integration` was asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum IntegrationArg {
    On,
    Off,
}

/// Reads or flips the browser integration switch on a running engine.
async fn run_integration_command(choice: Option<IntegrationArg>) -> std::process::ExitCode {
    use idm_ipc::{IpcClient, Request, Response};

    let mut client = match IpcClient::connect("cli").await {
        Ok(client) => client,
        Err(error) => {
            eprintln!("error: no running engine ({error}). Start one with: idm --serve");
            return std::process::ExitCode::FAILURE;
        }
    };

    // Read first even when setting: the engine owns the rest of the settings
    // and this must not reset them to whatever defaults the CLI holds.
    let mut settings = match client.request(Request::GetSettings).await {
        Ok(Response::Settings(settings)) => settings,
        Ok(other) => {
            eprintln!("unexpected reply: {other:?}");
            return std::process::ExitCode::FAILURE;
        }
        Err(error) => {
            eprintln!("error: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let wanted = match choice {
        Some(IntegrationArg::On) => true,
        Some(IntegrationArg::Off) => false,
        // `--integration` with no value: report, change nothing.
        None => {
            eprintln!("browser integration is {}", on_off(settings.browser_integration));
            return std::process::ExitCode::SUCCESS;
        }
    };

    if settings.browser_integration == wanted {
        eprintln!("browser integration is already {}", on_off(wanted));
        return std::process::ExitCode::SUCCESS;
    }

    settings.browser_integration = wanted;
    match client.request(Request::SetSettings(settings)).await {
        Ok(Response::Settings(applied)) => {
            eprintln!("browser integration is now {}", on_off(applied.browser_integration));
            std::process::ExitCode::SUCCESS
        }
        Ok(Response::Pong) => {
            eprintln!("browser integration is now {}", on_off(wanted));
            std::process::ExitCode::SUCCESS
        }
        Ok(Response::Rejected { reason }) => {
            eprintln!("refused: {reason}");
            std::process::ExitCode::FAILURE
        }
        Ok(other) => {
            eprintln!("unexpected reply: {other:?}");
            std::process::ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("error: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

async fn run_pairing_command(args: &Args) -> std::process::ExitCode {
    use idm_ipc::{IpcClient, Request, Response};

    // Connecting as "cli" rather than the browser name matters: the engine
    // refuses pairing management from browser clients.
    let mut client = match IpcClient::connect("cli").await {
        Ok(client) => client,
        Err(error) => {
            eprintln!("error: no running engine ({error}). Start one with: idm --serve");
            return std::process::ExitCode::FAILURE;
        }
    };

    let request = if let Some(id) = &args.approve {
        Request::ApprovePairing { request_id: id.clone() }
    } else if let Some(id) = &args.deny {
        Request::DenyPairing { request_id: id.clone() }
    } else if let Some(id) = &args.revoke {
        Request::RevokePairing { client_id: id.clone() }
    } else {
        Request::ListPairings
    };

    match client.request(request).await {
        Ok(Response::Pairings { paired, pending }) => {
            if pending.is_empty() {
                eprintln!("no pending requests");
            } else {
                eprintln!("pending:");
                for entry in pending {
                    eprintln!("  {}  {}", entry.request_id, entry.label);
                }
            }
            if paired.is_empty() {
                eprintln!("no paired extensions");
            } else {
                eprintln!("paired:");
                for entry in paired {
                    eprintln!("  {}  {}", entry.client_id, entry.label);
                }
            }
            std::process::ExitCode::SUCCESS
        }
        Ok(Response::PairingApproved) => {
            eprintln!("approved");
            std::process::ExitCode::SUCCESS
        }
        Ok(Response::Pong) => {
            eprintln!("done");
            std::process::ExitCode::SUCCESS
        }
        Ok(Response::Rejected { reason }) => {
            eprintln!("refused: {reason}");
            std::process::ExitCode::FAILURE
        }
        Ok(other) => {
            eprintln!("unexpected reply: {other:?}");
            std::process::ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("error: {error}");
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

/// Downloads over FTP or FTPS.
async fn download_ftp(url: &str, args: &Args, show_progress: bool) -> std::process::ExitCode {
    let mut options = idm_net::ftp::FtpOptions::new(args.out.clone());
    options.filename = args.name.clone();
    options.username = args.user.clone();
    options.password = args.password.clone();

    let quiet = args.quiet;
    let outcome = idm_net::ftp::download(url, &options, move |done, total| {
        if show_progress {
            match total {
                Some(total) if total > 0 => eprint!(
                    "\r  {} of {}  ({:.0}%)        ",
                    human_bytes(done),
                    human_bytes(total),
                    done as f64 / total as f64 * 100.0
                ),
                _ => eprint!("\r  {}        ", human_bytes(done)),
            }
            let _ = std::io::stderr().flush();
        }
    })
    .await;

    if show_progress {
        eprintln!();
    }

    match outcome {
        Ok(report) => {
            if !quiet {
                let how = if report.secure { "FTPS" } else { "FTP" };
                let resumed = if report.resumed_bytes > 0 {
                    format!(", {} already on disk", human_bytes(report.resumed_bytes))
                } else {
                    String::new()
                };
                eprintln!(
                    "{} — {} over {how}{resumed}",
                    report.path.display(),
                    human_bytes(report.bytes)
                );
            }
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Downloads a stream: manifest, rendition, segments, one file.
async fn download_stream(url: &str, args: &Args, show_progress: bool) -> std::process::ExitCode {
    let mut options = idm_net::stream::StreamOptions::new(args.out.clone());
    options.filename = args.name.clone();
    options.referrer = args.referrer.clone();
    options.cookies = args.cookies.clone();
    options.max_height = args.max_height;

    let quiet = args.quiet;
    let outcome = idm_net::stream::download(url, &options, move |progress| {
        if show_progress && progress.segments_total > 0 {
            eprint!(
                "\r  {}/{} segments  {}        ",
                progress.segments_done,
                progress.segments_total,
                human_bytes(progress.bytes)
            );
            let _ = std::io::stderr().flush();
        }
    })
    .await;

    if show_progress {
        eprintln!();
    }

    match outcome {
        Ok(report) => {
            if !quiet {
                eprintln!(
                    "{} — {} from {} segments ({})",
                    report.path.display(),
                    human_bytes(report.bytes),
                    report.segments,
                    report.label
                );
                // Said plainly rather than left for the user to discover: the
                // file is playable, just not in the shape they expected.
                if let Some(note) = &report.note {
                    eprintln!("note: {note}");
                }
            }
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
