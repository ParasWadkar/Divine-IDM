//! Fetching the video behind a page, by handing the page to yt-dlp.
//!
//! A video page is not a file. The engine's own path probes an address,
//! finds no length and no ranges, and correctly refuses -- there is nothing
//! there to fetch in parallel. What the person wanted is the video the page
//! is about, and working out which of a site's dozen stream URLs that is,
//! for a thousand sites that each change quarterly, is yt-dlp's whole job.
//! Ghost Downloader reaches the same conclusion and accepts yt-dlp tasks
//! rather than writing extractors; so does this.
//!
//! `Tool::YtDlp` has been in the sidecar list since sidecars existed --
//! fetchable, pinned, checksummed, version-probed -- and nothing ever ran
//! it. This is what runs it.
//!
//! # Only when it is already there
//!
//! Nothing here installs anything. If yt-dlp has not been fetched -- in
//! Settings, deliberately, like ffmpeg -- this reports that and the
//! download fails with the refusal it would have had anyway. A download
//! manager that silently downloads a program in order to perform a download
//! is not a trade anybody agreed to.
//!
//! # Parsing its progress
//!
//! `--progress-template` with a prefix of our own, one line at a time, and
//! never the human progress bar: the bar redraws with carriage returns and
//! is explicitly not a format to parse. The field names come from yt-dlp's
//! own progress hook -- `downloaded_bytes` is always there while a download
//! runs, `total_bytes` and `total_bytes_estimate` may each be absent, and
//! anything absent renders as `NA` rather than being omitted, which is why
//! the parser treats `NA` as "not known" rather than as a number it failed
//! to read.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use idm_media::sidecar::{self, Tool};
use tokio::io::{AsyncBufReadExt, BufReader};

/// The marker our progress lines start with.
///
/// yt-dlp writes plenty else to stdout -- the destination, post-processing,
/// warnings -- so the lines meant for us say so instead of being recognised
/// by shape.
const MARKER: &str = "idmprogress";

/// What yt-dlp prints for a template field it has no value for.
const UNKNOWN: &str = "NA";

#[derive(Debug, thiserror::Error)]
pub enum VideoError {
    #[error("{0}")]
    Missing(String),
    #[error("yt-dlp failed: {0}")]
    Failed(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// How far along a fetch is.
///
/// `total` is an `Option` and not a zero, because "no total" and "a total of
/// nothing" want opposite things drawn: a bar that cannot be drawn versus an
/// empty one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub downloaded: u64,
    pub total: Option<u64>,
    pub finished: bool,
}

/// What came back.
#[derive(Debug, Clone)]
pub struct Fetched {
    pub path: PathBuf,
    pub bytes: u64,
}

/// One progress line, or `None` for any other output.
///
/// Split rather than parsed with a regular expression so a line with a
/// field missing is still read as far as it goes: yt-dlp is not a stable
/// interface, and a download that keeps working with no progress bar is a
/// better failure than one that stops because a number moved.
pub fn read_progress(line: &str) -> Option<Progress> {
    let rest = line.trim().strip_prefix(MARKER)?;
    let mut fields = rest.split_whitespace();
    let status = fields.next()?;
    let downloaded = number(fields.next()?)?;
    // Whichever of the two totals it has, preferring the real one. An
    // estimate is still worth a progress bar; being told the size twice,
    // once as a guess, is not worth refusing to draw one.
    let total = number(fields.next().unwrap_or(UNKNOWN))
        .or_else(|| number(fields.next().unwrap_or(UNKNOWN)));
    Some(Progress { downloaded, total, finished: status == "finished" })
}

fn number(field: &str) -> Option<u64> {
    if field == UNKNOWN || field.is_empty() {
        return None;
    }
    // yt-dlp prints byte counts as integers, but a float here would be the
    // sort of thing that changes in a release, so take the whole part.
    field.split('.').next()?.parse().ok()
}

/// What to fetch, and what the page needed in order to be read.
///
/// A struct rather than six parameters, and the referrer and the cookies
/// are the reason it exists. The engine has carried both since the browser
/// extension did -- a video behind a sign-in is the ordinary case, not the
/// exotic one -- and this path dropped them on the floor, so handing a page
/// to yt-dlp lost exactly the credentials that made the page readable. The
/// download then failed with whatever the site shows a stranger, which
/// looks like yt-dlp not supporting the site.
#[derive(Debug, Clone)]
pub struct Wanted {
    pub url: String,
    pub output_dir: PathBuf,
    /// The name to save under, or `None` to take the video's title.
    pub filename: Option<String>,
    pub referrer: Option<String>,
    /// A `Cookie` header value, as the extension and the UI supply it.
    pub cookies: Option<String>,
}

impl Wanted {
    pub fn new(url: impl Into<String>, output_dir: impl Into<PathBuf>) -> Self {
        Self {
            url: url.into(),
            output_dir: output_dir.into(),
            filename: None,
            referrer: None,
            cookies: None,
        }
    }
}

/// The arguments for one fetch.
///
/// Built separately from running it so the whole command line is something
/// a test can look at. Nothing is passed through a shell: the address comes
/// from the person and goes in as one argument, where no amount of quoting
/// in it can become a second one.
pub fn arguments(wanted: &Wanted) -> Vec<String> {
    let template = match wanted.filename.as_deref() {
        // An extension yt-dlp chooses, because the container depends on
        // which streams it ends up merging and a name ending `.mp4` on a
        // `.webm` would be a lie this program tells about its own output.
        Some(name) => format!("{}.%(ext)s", strip_extension(name)),
        None => "%(title)s.%(ext)s".to_owned(),
    };

    let mut args = vec![
        // One line per update, never the redrawing bar.
        "--newline".to_owned(),
        "--no-colors".to_owned(),
        // Without this there is no progress at all. `--print`, below,
        // implies `--quiet`, and quiet silences the progress template
        // along with everything else -- so the download runs, succeeds,
        // and reports nothing from start to finish. The window showed
        // 0 B at 0 B/s for the whole of a six-megabyte fetch and then
        // jumped to done, which is how this was found.
        "--progress".to_owned(),
        "--progress-template".to_owned(),
        format!(
            "download:{MARKER} %(progress.status)s %(progress.downloaded_bytes)s \
             %(progress.total_bytes)s %(progress.total_bytes_estimate)s"
        ),
        // A page that is both a video and a playlist means the video. The
        // alternative is that one address quietly becomes ninety downloads.
        "--no-playlist".to_owned(),
        // Where the finished file lands. `-P` rather than an absolute `-o`,
        // because an absolute output template makes yt-dlp ignore `-P` and
        // the two would then disagree about the destination.
        "-P".to_owned(),
        wanted.output_dir.display().to_string(),
        "-o".to_owned(),
        template,
        // The real path, after any merging has renamed it. `--print` alone
        // implies `--simulate`, which would print a name and fetch nothing;
        // `--no-simulate` is what keeps it a download.
        "--no-simulate".to_owned(),
        "--print".to_owned(),
        "after_move:filepath".to_owned(),
    ];

    // What the browser sent when it loaded the page. Sites that check it do
    // so on the page, not on the stream, so this has to be on the request
    // that does the extracting.
    if let Some(referrer) = &wanted.referrer {
        args.push("--referer".to_owned());
        args.push(referrer.clone());
    }

    // As a header rather than through `--cookies`, because a header is what
    // we have: one `Cookie:` line, from the extension or typed in. Writing
    // it out as a Netscape cookie file would mean inventing a domain, a
    // path and an expiry for every pair and leaving the person's session on
    // disk for yt-dlp to read -- worse on both counts. yt-dlp calls this
    // deprecated and scopes the cookies to the address being downloaded,
    // which is the scoping we would have wanted anyway.
    if let Some(cookies) = &wanted.cookies {
        args.push("--add-header".to_owned());
        args.push(format!("Cookie:{cookies}"));
    }

    // Last, and after `--`, so an address beginning with a dash is an
    // address rather than an option.
    args.push("--".to_owned());
    args.push(wanted.url.clone());
    args
}

fn strip_extension(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.len() <= 5 => stem,
        _ => name,
    }
}

/// Runs yt-dlp against `url`, reporting progress, and says where the file
/// landed.
///
/// `program` is taken rather than found so a test can point this at
/// something that behaves like yt-dlp without one being installed.
pub async fn fetch_with(
    program: &Path,
    wanted: &Wanted,
    mut on_progress: impl FnMut(Progress),
) -> Result<Fetched, VideoError> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(arguments(wanted))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        // No console flashing up behind the window for each fetch.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = command.spawn()?;
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");

    // Drained on its own task. yt-dlp is chatty on stderr, and a full pipe
    // nobody is reading stops the process rather than the logging.
    let complaints = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        let mut kept = Vec::new();
        while let Ok(Some(line)) = lines.next_line().await {
            if kept.len() < 20 {
                kept.push(line);
            }
        }
        kept
    });

    let mut landed: Option<PathBuf> = None;
    let mut seen = Progress { downloaded: 0, total: None, finished: false };
    let mut lines = BufReader::new(stdout).lines();
    while let Some(line) = lines.next_line().await? {
        if let Some(progress) = read_progress(&line) {
            seen = progress;
            on_progress(progress);
        } else if !line.trim().is_empty() {
            // The last non-progress line that looks like a path is the one
            // `--print after_move:filepath` wrote. Taken as the last rather
            // than the first, because post-processing can rename the file
            // after it has already been announced.
            let candidate = Path::new(line.trim());
            if candidate.is_absolute() {
                landed = Some(candidate.to_path_buf());
            }
        }
    }

    let status = child.wait().await?;
    let said = complaints.await.unwrap_or_default();
    if !status.success() {
        let why = said.last().cloned().unwrap_or_else(|| format!("exit {status}"));
        return Err(VideoError::Failed(why));
    }

    let path = landed.ok_or_else(|| {
        VideoError::Failed("it finished without saying where it put the file".to_owned())
    })?;
    // The size from disk, not from the progress line: with a merge the
    // finished file is not the sum of what was reported.
    let bytes = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(seen.downloaded);
    Ok(Fetched { path, bytes })
}

/// The managed yt-dlp, or a refusal naming what is missing.
pub fn find(sidecar_dir: &Path) -> Result<PathBuf, VideoError> {
    sidecar::probe_managed(Tool::YtDlp, sidecar_dir)
        .map(|found| found.path)
        .map_err(|error| VideoError::Missing(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain() -> Wanted {
        Wanted::new("https://x.invalid/v", "D:/out")
    }

    #[test]
    fn a_progress_line_is_read() {
        let got = read_progress("idmprogress downloading 1024 4096 4096").unwrap();
        assert_eq!(got, Progress { downloaded: 1024, total: Some(4096), finished: false });
    }

    #[test]
    fn an_unknown_total_is_not_a_total_of_zero() {
        // A live stream has no size. Drawing a full bar, or an empty one,
        // would both be claims nobody measured -- so the total is absent
        // and the caller decides what to show instead.
        let got = read_progress("idmprogress downloading 2048 NA NA").unwrap();
        assert_eq!(got.downloaded, 2048);
        assert_eq!(got.total, None);
    }

    #[test]
    fn an_estimate_counts_when_the_real_total_is_missing() {
        let got = read_progress("idmprogress downloading 10 NA 500").unwrap();
        assert_eq!(got.total, Some(500), "an estimated size still draws a bar");
    }

    #[test]
    fn the_finished_line_says_so() {
        let got = read_progress("idmprogress finished 4096 4096 4096").unwrap();
        assert!(got.finished);
    }

    #[test]
    fn anything_else_yt_dlp_prints_is_not_progress() {
        for line in [
            "[youtube] Extracting URL: https://example.invalid/watch",
            "[download] Destination: C:\\x\\video.mp4",
            "C:\\x\\video.mp4",
            "",
        ] {
            assert!(read_progress(line).is_none(), "{line:?}");
        }
    }

    #[test]
    fn a_float_byte_count_is_still_a_byte_count() {
        // Not what it prints today. It is the sort of thing that changes in
        // a release, and losing the progress bar over it would be silly.
        assert_eq!(read_progress("idmprogress downloading 1024.0 NA NA").unwrap().downloaded, 1024);
    }

    #[test]
    fn the_address_cannot_become_an_option() {
        // `--` before it, so a file named like a flag is still a file.
        let args = arguments(&Wanted::new("-oh-dear", "D:/out"));
        let dashes = args.iter().position(|a| a == "--").expect("a -- separator");
        assert_eq!(args[dashes + 1], "-oh-dear");
        assert_eq!(args.len(), dashes + 2, "the address is the last argument");
    }

    #[test]
    fn a_chosen_name_keeps_its_stem_and_lets_yt_dlp_pick_the_container() {
        // The extension depends on which streams get merged, so claiming
        // one up front would be this program lying about its own output.
        let args = arguments(&Wanted { filename: Some("holiday.mp4".to_owned()), ..plain() });
        let template = &args[args.iter().position(|a| a == "-o").unwrap() + 1];
        assert_eq!(template, "holiday.%(ext)s");
    }

    #[test]
    fn a_name_with_no_extension_is_left_alone() {
        let args = arguments(&Wanted { filename: Some("holiday".to_owned()), ..plain() });
        let template = &args[args.iter().position(|a| a == "-o").unwrap() + 1];
        assert_eq!(template, "holiday.%(ext)s");
    }

    #[test]
    fn it_asks_for_one_video_rather_than_a_playlist() {
        // One address quietly becoming ninety downloads is the surprise
        // this avoids.
        let args = arguments(&plain());
        assert!(args.iter().any(|a| a == "--no-playlist"));
    }

    #[test]
    fn progress_is_asked_for_loudly_enough_to_survive_quiet() {
        // `--print` implies `--quiet`, and quiet silences the progress
        // template too. Without `--progress` the only output is the final
        // path: the download works and reports nothing, which looks like a
        // stalled download for as long as it runs.
        let args = arguments(&plain());
        assert!(args.iter().any(|a| a == "--progress"), "{args:?}");
        assert!(args.iter().any(|a| a == "--newline"));
    }

    #[test]
    fn printing_the_path_does_not_turn_the_download_into_a_rehearsal() {
        // `--print` implies `--simulate`, which would print a filename and
        // fetch nothing at all.
        let args = arguments(&plain());
        assert!(args.iter().any(|a| a == "--no-simulate"));
        assert!(args.iter().any(|a| a == "after_move:filepath"));
    }

    #[test]
    fn the_referrer_the_page_needed_is_passed_on() {
        // A site that gates its pages on the referrer gates the page, which
        // is the request yt-dlp makes to find the video at all.
        let args =
            arguments(&Wanted { referrer: Some("https://x.invalid/watch".to_owned()), ..plain() });
        let at = args.iter().position(|a| a == "--referer").expect("a --referer");
        assert_eq!(args[at + 1], "https://x.invalid/watch");
    }

    #[test]
    fn the_cookies_the_page_needed_are_passed_on() {
        // The case this exists for: a video behind a sign-in. Without the
        // cookies yt-dlp is shown whatever a stranger is shown, and the
        // failure reads as yt-dlp not supporting the site.
        let args =
            arguments(&Wanted { cookies: Some("session=abc; lang=en".to_owned()), ..plain() });
        let at = args.iter().position(|a| a == "--add-header").expect("an --add-header");
        assert_eq!(args[at + 1], "Cookie:session=abc; lang=en");
    }

    #[test]
    fn neither_appears_when_there_is_nothing_to_say() {
        // An empty `--referer` is not the same as no referrer, and a bare
        // `Cookie:` header is a request to be treated as signed out by a
        // site that would otherwise not have looked.
        let args = arguments(&plain());
        assert!(!args.iter().any(|a| a == "--referer"), "{args:?}");
        assert!(!args.iter().any(|a| a == "--add-header"), "{args:?}");
    }

    #[test]
    fn credentials_still_cannot_turn_into_options() {
        // Everything added goes before the `--`, so the address stays the
        // last argument and a cookie value starting with a dash is a value.
        let args = arguments(&Wanted {
            referrer: Some("-r".to_owned()),
            cookies: Some("-c".to_owned()),
            ..plain()
        });
        let dashes = args.iter().position(|a| a == "--").expect("a -- separator");
        assert_eq!(args.len(), dashes + 2, "the address is the last argument: {args:?}");
        assert_eq!(args[dashes + 1], "https://x.invalid/v");
    }
}
