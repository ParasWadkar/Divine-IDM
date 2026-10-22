//! Streaming manifests, and the sidecars that turn what they describe into a
//! file.
//!
//! A stream is not one download. It is a manifest listing hundreds of small
//! segments, often with audio in a second list, and the job is to work out
//! which bytes to fetch and how to put them back together.
//!
//! Nothing in this crate opens a socket. Manifests go in as text and come
//! out as structures, so every strange shape a real server might send can be
//! tested without one — and the strange shapes are what break this, far more
//! often than the network does. Fetching belongs to `idm-net`, which already
//! knows how to do it quickly.

pub mod assemble;
pub mod dash;
pub mod hls;
pub mod sidecar;

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("this manifest could not be read: {0}")]
    Manifest(String),
    #[error("expected a master playlist, but this is a media playlist")]
    NotAMaster,
    #[error("expected a media playlist, but this is a master playlist")]
    NotAMedia,
    #[error("{0} is not a usable address")]
    BadUrl(String),
    #[error("this stream lists no segments")]
    Empty,
    #[error("{0}")]
    Unsupported(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Sidecar(String),
}

pub type Result<T> = std::result::Result<T, MediaError>;

/// What a caller has to fetch, and how to join it afterwards.
///
/// The common shape across HLS and DASH: an optional initialisation blob,
/// then segments in order, possibly twice over when audio is carried
/// separately from video.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub video: Track,
    /// Present only when audio is a separate list, which DASH always does
    /// and HLS does whenever a variant names an audio group.
    pub audio: Option<Track>,
    /// Whether the manifest will keep growing while this is downloaded.
    pub live: bool,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub init: Option<String>,
    pub segments: Vec<String>,
    /// What the joined segments will be: `ts`, `mp4`, `m4a`, `webm`.
    pub container: &'static str,
}

impl Plan {
    /// Every URL that has to be fetched, in the order they are needed.
    pub fn urls(&self) -> Vec<String> {
        let mut urls = Vec::new();
        for track in std::iter::once(&self.video).chain(self.audio.iter()) {
            urls.extend(track.init.iter().cloned());
            urls.extend(track.segments.iter().cloned());
        }
        urls
    }

    pub fn segment_count(&self) -> usize {
        self.video.segments.len() + self.audio.as_ref().map_or(0, |a| a.segments.len())
    }

    /// Whether finishing this needs ffmpeg.
    ///
    /// Joining segments of one track is concatenation, which needs nothing.
    /// Putting two tracks into one file is multiplexing, which does.
    pub fn needs_muxing(&self) -> bool {
        self.audio.is_some()
    }
}

/// Guesses a container from a segment address.
///
/// The address is a hint rather than a promise — a server can serve anything
/// under any name — but it is what every player uses and it is right nearly
/// always. The joined file is identified properly later, by its own bytes,
/// if ffmpeg is asked to remux it.
pub fn container_for(url: &str) -> &'static str {
    let path = url.split(['?', '#']).next().unwrap_or(url).to_ascii_lowercase();
    if path.ends_with(".ts") {
        "ts"
    } else if path.ends_with(".m4s") || path.ends_with(".mp4") || path.ends_with(".m4v") {
        "mp4"
    } else if path.ends_with(".m4a") || path.ends_with(".aac") {
        "m4a"
    } else if path.ends_with(".webm") {
        "webm"
    } else {
        // MPEG-TS is what an unlabelled HLS segment almost always is.
        "ts"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn containers_are_read_from_the_address_and_survive_a_query_string() {
        assert_eq!(container_for("https://x/seg1.ts"), "ts");
        assert_eq!(container_for("https://x/seg1.m4s?token=abc"), "mp4");
        assert_eq!(container_for("https://x/audio.m4a#frag"), "m4a");
        assert_eq!(container_for("https://x/v.WEBM"), "webm");
        assert_eq!(container_for("https://x/segment"), "ts", "unlabelled segments are TS");
    }

    #[test]
    fn a_plan_lists_initialisation_before_the_segments_that_need_it() {
        let plan = Plan {
            video: Track {
                init: Some("https://x/init.mp4".into()),
                segments: vec!["https://x/1.m4s".into(), "https://x/2.m4s".into()],
                container: "mp4",
            },
            audio: None,
            live: false,
            label: "1080p".into(),
        };
        assert_eq!(plan.urls()[0], "https://x/init.mp4");
        assert_eq!(plan.segment_count(), 2);
        assert!(!plan.needs_muxing(), "one track joins without ffmpeg");
    }

    #[test]
    fn two_tracks_need_muxing_and_one_does_not() {
        let track = |name: &str| Track {
            init: None,
            segments: vec![format!("https://x/{name}.ts")],
            container: "ts",
        };
        let mut plan = Plan { video: track("v"), audio: None, live: false, label: "720p".into() };
        assert!(!plan.needs_muxing());

        plan.audio = Some(track("a"));
        assert!(plan.needs_muxing(), "separate audio has to be combined");
        assert_eq!(plan.urls().len(), 2);
    }
}
