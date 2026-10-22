//! Downloading a stream: fetch the manifest, pick a rendition, fetch its
//! segments, put them back together.
//!
//! The split from `idm-media` is deliberate. That crate turns manifests into
//! structures and never opens a socket, so every strange manifest can be
//! tested as text. This is where the sockets live.
//!
//! Segments are fetched several at a time rather than through the per-file
//! scheduler. That scheduler exists to split *one large file* across many
//! connections, and it would be wasted on a two-megabyte segment: the win
//! here comes from having several segments in flight, which is the same idea
//! one level up. A single-file DASH rendition is the exception and goes
//! through the ordinary download path, where ranged parallelism does apply.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use idm_media::sidecar::{self, Tool};
use idm_media::{Plan, Track, assemble, container_for, dash, hls};
use tokio::sync::Semaphore;

use crate::client::HttpConfig;

#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error(transparent)]
    Media(#[from] idm_media::MediaError),
    #[error("http: {0}")]
    Http(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Refused(String),
}

type Result<T> = std::result::Result<T, StreamError>;

#[derive(Debug, Clone)]
pub struct StreamOptions {
    pub output_dir: PathBuf,
    /// Overrides the name derived from the manifest address.
    pub filename: Option<String>,
    /// Picks the best rendition no taller than this.
    pub max_height: Option<u64>,
    /// Segments in flight at once.
    pub concurrency: usize,
    pub referrer: Option<String>,
    pub cookies: Option<String>,
}

impl StreamOptions {
    pub fn new(output_dir: impl Into<PathBuf>) -> Self {
        Self {
            output_dir: output_dir.into(),
            filename: None,
            max_height: None,
            // Enough to keep the link busy on segments of a few megabytes,
            // few enough not to look like an attack to the origin.
            concurrency: 8,
            referrer: None,
            cookies: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct StreamReport {
    pub path: PathBuf,
    pub bytes: u64,
    pub segments: usize,
    pub label: String,
    /// What finishing it needed, and whether that happened.
    pub muxed: bool,
    /// Said plainly when the result is playable but not in the container the
    /// user would have expected.
    pub note: Option<String>,
}

/// Whether an address looks like a stream manifest rather than a file.
///
/// The extension is a hint; the content type settles it, and that is only
/// known after a request. Used to decide whether to look at all.
pub fn looks_like_manifest(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or(url).to_ascii_lowercase();
    path.ends_with(".m3u8") || path.ends_with(".m3u") || path.ends_with(".mpd")
}

/// Whether a response body is a manifest, by what it contains.
///
/// Content type is unreliable here — plenty of servers hand out `.m3u8` as
/// `text/plain` or `application/octet-stream` — so the first line decides.
pub fn sniff(body: &str, url: &str) -> Option<Kind> {
    let head = body.trim_start();
    if head.starts_with("#EXTM3U") {
        return Some(Kind::Hls);
    }
    if head.starts_with("<?xml") || head.starts_with("<MPD") {
        return head.contains("<MPD").then_some(Kind::Dash);
    }
    // Nothing recognisable in the body; fall back to the address.
    looks_like_manifest(url)
        .then(|| if url.to_ascii_lowercase().contains(".mpd") { Kind::Dash } else { Kind::Hls })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Hls,
    Dash,
}

/// Progress across a whole stream download.
#[derive(Debug, Clone, Copy)]
pub struct StreamProgress {
    pub segments_done: usize,
    pub segments_total: usize,
    pub bytes: u64,
}

/// Works out what to fetch for a stream, without fetching it.
///
/// Separate from the download so a caller can show the user what it found —
/// which rendition, how many segments, whether it is live — before anything
/// large happens.
pub async fn plan(url: &str, options: &StreamOptions) -> Result<Plan> {
    let client = manifest_client(options)?;
    let body = get_text(&client, url, options).await?;

    match sniff(&body, url)
        .ok_or_else(|| StreamError::Refused("that address is not a stream manifest".to_owned()))?
    {
        Kind::Hls => plan_hls(&client, &body, url, options).await,
        Kind::Dash => plan_dash(&body, url, options),
    }
}

async fn plan_hls(
    client: &reqwest::Client,
    body: &str,
    url: &str,
    options: &StreamOptions,
) -> Result<Plan> {
    // A master playlist lists renditions; a media playlist is already one.
    if let Ok(master) = hls::parse_master(body, url) {
        let variant = match options.max_height {
            Some(height) => master.at_most(height),
            None => master.best(),
        }
        .ok_or(idm_media::MediaError::Empty)?
        .clone();

        let audio = master.audio_for(&variant).and_then(|r| r.url.clone());
        let media_body = get_text(client, &variant.url, options).await?;
        let media = hls::parse_media(&media_body, &variant.url)?;
        media.is_downloadable()?;

        let audio_track = match audio {
            Some(audio_url) => {
                let audio_body = get_text(client, &audio_url, options).await?;
                let audio_media = hls::parse_media(&audio_body, &audio_url)?;
                audio_media.is_downloadable()?;
                Some(track_from(&audio_media))
            }
            None => None,
        };

        return Ok(Plan {
            video: track_from(&media),
            audio: audio_track,
            live: media.live,
            label: variant.label(),
        });
    }

    let media = hls::parse_media(body, url)?;
    media.is_downloadable()?;
    Ok(Plan { video: track_from(&media), audio: None, live: media.live, label: "stream".into() })
}

fn plan_dash(body: &str, url: &str, options: &StreamOptions) -> Result<Plan> {
    let manifest = dash::parse(body, url)?;
    let video = match options.max_height {
        Some(height) => manifest.at_most(height),
        None => manifest.best_video(),
    }
    .ok_or(idm_media::MediaError::Empty)?;

    let audio = manifest.best_audio().map(|audio| Track {
        init: audio.init.clone(),
        segments: audio.segments.clone(),
        container: audio.segments.first().map_or("m4a", |u| container_for(u)),
    });

    Ok(Plan {
        video: Track {
            init: video.init.clone(),
            segments: video.segments.clone(),
            container: video.segments.first().map_or("mp4", |u| container_for(u)),
        },
        audio,
        live: manifest.live,
        label: video.label(),
    })
}

fn track_from(media: &hls::Media) -> Track {
    let segments: Vec<String> = media.segments.iter().map(|s| s.url.clone()).collect();
    Track {
        container: segments.first().map_or("ts", |u| container_for(u)),
        init: media.init.clone(),
        segments,
    }
}

/// Fetches a planned stream and assembles it.
pub async fn download<F>(
    url: &str,
    options: &StreamOptions,
    mut progress: F,
) -> Result<StreamReport>
where
    F: FnMut(StreamProgress) + Send + 'static,
{
    let plan = plan(url, options).await?;

    if plan.live {
        // Recording a live stream is a different job with a stop control,
        // not a download with an end. Refusing beats appearing to work and
        // never finishing.
        return Err(StreamError::Refused(
            "this is a live stream; recording one is not supported yet".to_owned(),
        ));
    }

    let stem = options
        .filename
        .clone()
        .map(|name| name.trim_end_matches(".mp4").to_owned())
        .unwrap_or_else(|| stem_from(url));

    std::fs::create_dir_all(&options.output_dir)?;
    let scratch = options.output_dir.join(format!(".{stem}.parts"));
    std::fs::create_dir_all(&scratch)?;

    let client = manifest_client(options)?;
    let total = plan.segment_count();
    let done = Arc::new(AtomicU64::new(0));
    let bytes = Arc::new(AtomicU64::new(0));

    let video_parts = fetch_track(
        &client,
        &plan.video,
        &scratch,
        "v",
        options,
        &done,
        &bytes,
        total,
        &mut progress,
    )
    .await?;
    let audio_parts = match &plan.audio {
        Some(audio) => Some(
            fetch_track(
                &client,
                audio,
                &scratch,
                "a",
                options,
                &done,
                &bytes,
                total,
                &mut progress,
            )
            .await?,
        ),
        None => None,
    };

    // Joining is concatenation, which needs nothing installed.
    let video_file = assemble::part_name(&scratch, &stem, "video", plan.video.container);
    assemble::concatenate(&video_parts, &video_file)?;

    let audio_file = match (&plan.audio, &audio_parts) {
        (Some(audio), Some(parts)) => {
            let path = assemble::part_name(&scratch, &stem, "audio", audio.container);
            assemble::concatenate(parts, &path)?;
            Some(path)
        }
        _ => None,
    };

    let (path, muxed, note) = finish(&stem, options, &plan, &video_file, audio_file.as_deref())?;

    let written = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let _ = std::fs::remove_dir_all(&scratch);

    Ok(StreamReport { path, bytes: written, segments: total, label: plan.label, muxed, note })
}

/// Produces the final file, muxing when that is needed and possible.
///
/// When ffmpeg is missing the download is still delivered rather than
/// failed: one track is already playable, and two tracks are two playable
/// files. Saying so is more use than discarding work that is already done.
fn finish(
    stem: &str,
    options: &StreamOptions,
    plan: &Plan,
    video: &Path,
    audio: Option<&Path>,
) -> Result<(PathBuf, bool, Option<String>)> {
    let wants = plan.needs_muxing() || assemble::wants_remux(plan.video.container);
    let target = options.output_dir.join(format!("{stem}.mp4"));

    if !wants {
        let plain = options.output_dir.join(format!("{stem}.{}", plan.video.container));
        std::fs::rename(video, &plain)?;
        return Ok((plain, false, None));
    }

    match sidecar::probe(Tool::Ffmpeg, Some(&options.output_dir)) {
        Ok(ffmpeg) => {
            ffmpeg.run(sidecar::mux_arguments(video, audio, &target))?;
            Ok((target, true, None))
        }
        Err(why) => {
            let kept = options.output_dir.join(format!("{stem}.{}", plan.video.container));
            std::fs::rename(video, &kept)?;

            let note = if let Some(audio) = audio {
                let audio_kept = options.output_dir.join(format!(
                    "{stem}.audio.{}",
                    plan.audio.as_ref().map_or("m4a", |a| a.container)
                ));
                std::fs::rename(audio, &audio_kept)?;
                format!(
                    "audio and video were downloaded separately and not combined, because {why}. \
                     Both files play on their own."
                )
            } else {
                format!("kept as .{} rather than .mp4, because {why}", plan.video.container)
            };
            Ok((kept, false, Some(note)))
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn fetch_track<F>(
    client: &reqwest::Client,
    track: &Track,
    scratch: &Path,
    prefix: &str,
    options: &StreamOptions,
    done: &Arc<AtomicU64>,
    bytes: &Arc<AtomicU64>,
    total: usize,
    progress: &mut F,
) -> Result<Vec<PathBuf>>
where
    F: FnMut(StreamProgress),
{
    let mut urls: Vec<(usize, String)> = Vec::new();
    if let Some(init) = &track.init {
        urls.push((0, init.clone()));
    }
    let offset = usize::from(track.init.is_some());
    urls.extend(track.segments.iter().cloned().enumerate().map(|(i, u)| (i + offset, u)));

    let limit = Arc::new(Semaphore::new(options.concurrency.max(1)));
    let mut tasks = Vec::with_capacity(urls.len());

    for (index, url) in urls {
        // Numbered by position, not by the server's name for it: two
        // segments can share a filename, and joining them in the wrong order
        // produces a file that plays and is wrong.
        let path = scratch.join(format!("{prefix}{index:06}"));
        let client = client.clone();
        let limit = Arc::clone(&limit);
        let done = Arc::clone(done);
        let bytes = Arc::clone(bytes);
        let referrer = options.referrer.clone();
        let cookies = options.cookies.clone();

        tasks.push(tokio::spawn(async move {
            let _permit = limit.acquire().await;
            let body = get_bytes(&client, &url, referrer.as_deref(), cookies.as_deref()).await?;
            std::fs::write(&path, &body)?;
            done.fetch_add(1, Ordering::Relaxed);
            bytes.fetch_add(body.len() as u64, Ordering::Relaxed);
            Ok::<PathBuf, StreamError>(path)
        }));
    }

    let mut parts = Vec::with_capacity(tasks.len());
    for task in tasks {
        parts.push(task.await.map_err(|error| StreamError::Http(error.to_string()))??);
        progress(StreamProgress {
            segments_done: done.load(Ordering::Relaxed) as usize,
            segments_total: total,
            bytes: bytes.load(Ordering::Relaxed),
        });
    }
    Ok(parts)
}

fn manifest_client(options: &StreamOptions) -> Result<reqwest::Client> {
    let _ = options;
    let config = HttpConfig::default();
    reqwest::Client::builder()
        .user_agent(config.user_agent.clone())
        .build()
        .map_err(|error| StreamError::Http(error.to_string()))
}

async fn get_text(client: &reqwest::Client, url: &str, options: &StreamOptions) -> Result<String> {
    let body =
        get_bytes(client, url, options.referrer.as_deref(), options.cookies.as_deref()).await?;
    Ok(String::from_utf8_lossy(&body).into_owned())
}

async fn get_bytes(
    client: &reqwest::Client,
    url: &str,
    referrer: Option<&str>,
    cookies: Option<&str>,
) -> Result<Vec<u8>> {
    let mut request = client.get(url);
    if let Some(referrer) = referrer {
        request = request.header(reqwest::header::REFERER, referrer);
    }
    if let Some(cookies) = cookies {
        request = request.header(reqwest::header::COOKIE, cookies);
    }

    let response = request.send().await.map_err(|error| StreamError::Http(error.to_string()))?;
    if !response.status().is_success() {
        return Err(StreamError::Http(format!("{} for {url}", response.status())));
    }
    Ok(response.bytes().await.map_err(|error| StreamError::Http(error.to_string()))?.to_vec())
}

/// A name for the output, from the manifest address.
fn stem_from(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let last = path.rsplit('/').find(|part| !part.is_empty()).unwrap_or("stream");
    let stem = last.split('.').next().unwrap_or("stream");
    // `index` and `master` name the playlist, not the video; the directory
    // above it usually names the video.
    if matches!(stem, "index" | "master" | "playlist" | "manifest" | "") {
        let parent = path
            .trim_end_matches(|c| c != '/')
            .trim_end_matches('/')
            .rsplit('/')
            .find(|part| !part.is_empty())
            .unwrap_or("stream");
        return sanitise(parent);
    }
    sanitise(stem)
}

fn sanitise(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || matches!(c, '-' | '_' | ' ') { c } else { '_' })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').to_owned();
    if trimmed.is_empty() { "stream".to_owned() } else { trimmed }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifests_are_recognised_by_their_contents_first() {
        // Servers hand out .m3u8 as text/plain often enough that the body
        // has to be what decides.
        assert_eq!(sniff("#EXTM3U\n#EXT-X-VERSION:3", "https://x/a.bin"), Some(Kind::Hls));
        assert_eq!(sniff("<?xml version=\"1.0\"?><MPD/>", "https://x/a.bin"), Some(Kind::Dash));
        assert_eq!(sniff("  \n#EXTM3U", "https://x/a"), Some(Kind::Hls), "leading space is fine");
    }

    #[test]
    fn something_that_is_not_a_manifest_is_not_mistaken_for_one() {
        assert_eq!(sniff("just some text", "https://x/a.bin"), None);
        assert_eq!(sniff("<?xml version=\"1.0\"?><rss/>", "https://x/feed.xml"), None);
    }

    #[test]
    fn the_address_is_a_fallback_when_the_body_says_nothing() {
        assert_eq!(sniff("", "https://x/a.m3u8"), Some(Kind::Hls));
        assert_eq!(sniff("", "https://x/a.mpd"), Some(Kind::Dash));
    }

    #[test]
    fn addresses_that_look_like_manifests_are_spotted() {
        assert!(looks_like_manifest("https://x/master.m3u8?token=1"));
        assert!(looks_like_manifest("https://x/MANIFEST.MPD"));
        assert!(!looks_like_manifest("https://x/video.mp4"));
    }

    #[test]
    fn a_playlist_named_index_takes_its_name_from_the_directory() {
        // "index.mp4" would be a useless name for every video ever
        // downloaded.
        assert_eq!(stem_from("https://cdn.example/the-lecture/index.m3u8"), "the-lecture");
        assert_eq!(stem_from("https://cdn.example/x/master.m3u8"), "x");
        assert_eq!(stem_from("https://cdn.example/holiday-video.m3u8"), "holiday-video");
    }

    #[test]
    fn a_name_from_a_url_cannot_escape_the_output_directory() {
        // It came from a web page, like everything else here.
        for hostile in [
            "https://x/../../windows/system32/a.m3u8",
            "https://x/a:b.m3u8",
            "https://x/..%2f..%2fetc.m3u8",
        ] {
            let stem = stem_from(hostile);
            assert!(!stem.contains(['/', '\\', ':']), "{stem}");
            assert!(!stem.contains(".."), "{stem}");
        }
    }

    #[test]
    fn a_query_string_does_not_become_part_of_the_name() {
        assert_eq!(stem_from("https://x/video.m3u8?sig=abc&exp=123"), "video");
    }
}
