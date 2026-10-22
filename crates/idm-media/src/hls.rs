//! HLS playlists: what renditions exist, and which bytes make one up.
//!
//! Parsing only. Nothing here opens a socket, so every shape a real server
//! might send can be tested as text — which matters, because the failures
//! that bite are odd manifests rather than odd networks.

use url::Url;

use crate::{MediaError, Result};

/// One selectable rendition from a master playlist.
#[derive(Debug, Clone, PartialEq)]
pub struct Variant {
    /// Absolute, already resolved against the playlist it came from.
    pub url: String,
    pub bandwidth: u64,
    pub resolution: Option<(u64, u64)>,
    pub codecs: Option<String>,
    pub frame_rate: Option<f64>,
    /// The `AUDIO` group this video stream expects, when audio is separate.
    pub audio_group: Option<String>,
}

impl Variant {
    pub fn height(&self) -> Option<u64> {
        self.resolution.map(|(_, height)| height)
    }

    /// A short label for a picker: `1080p · 5.2 Mbps`.
    pub fn label(&self) -> String {
        let quality = match self.resolution {
            Some((_, height)) => format!("{height}p"),
            None => "audio only".to_owned(),
        };
        format!("{quality} · {}", human_bitrate(self.bandwidth))
    }
}

/// A separate audio or subtitle rendition.
#[derive(Debug, Clone, PartialEq)]
pub struct Rendition {
    pub group_id: String,
    pub name: String,
    pub language: Option<String>,
    pub url: Option<String>,
    pub default: bool,
    pub kind: RenditionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenditionKind {
    Audio,
    Subtitles,
    ClosedCaptions,
    Video,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Master {
    pub variants: Vec<Variant>,
    pub renditions: Vec<Rendition>,
}

impl Master {
    /// The highest-bandwidth video rendition.
    ///
    /// Bandwidth rather than resolution: a manifest can advertise two
    /// renditions at the same height and very different quality, and the
    /// declared bandwidth is the only thing that separates them.
    pub fn best(&self) -> Option<&Variant> {
        self.variants.iter().max_by_key(|variant| variant.bandwidth)
    }

    /// The best rendition no taller than `height`, or the smallest there is.
    ///
    /// Falling back to the smallest rather than to nothing: a request for
    /// 480p against a manifest offering only 720p should still download
    /// something, and the alternative is an error the user can do nothing
    /// about.
    pub fn at_most(&self, height: u64) -> Option<&Variant> {
        self.variants
            .iter()
            .filter(|variant| variant.height().is_some_and(|h| h <= height))
            .max_by_key(|variant| variant.bandwidth)
            .or_else(|| self.variants.iter().min_by_key(|variant| variant.bandwidth))
    }

    /// The audio rendition a variant expects, if its audio is separate.
    pub fn audio_for(&self, variant: &Variant) -> Option<&Rendition> {
        let group = variant.audio_group.as_deref()?;
        let in_group = || {
            self.renditions.iter().filter(|r| r.kind == RenditionKind::Audio && r.group_id == group)
        };
        in_group().find(|r| r.default).or_else(|| in_group().next())
    }
}

/// How a media playlist's segments are protected.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Encryption {
    #[default]
    None,
    /// Whole-segment AES-128-CBC. Decryptable once the key is fetched.
    Aes128 {
        key_url: String,
        /// Absent means the sequence number is the IV, per the specification.
        iv: Option<[u8; 16]>,
    },
    /// Sample-level encryption, and anything else a manifest declares.
    ///
    /// Kept as a distinct case rather than folded into an error so the
    /// caller can say precisely what it cannot do, and hand the stream to a
    /// player that can.
    Unsupported(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub url: String,
    pub duration: f64,
    /// Byte range within `url`, for playlists that pack segments into one
    /// file.
    pub byte_range: Option<(u64, u64)>,
    /// Media sequence number, which doubles as the AES-128 IV when the
    /// playlist does not give one.
    pub sequence: u64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Media {
    pub segments: Vec<Segment>,
    pub target_duration: f64,
    /// No `#EXT-X-ENDLIST`: the playlist is still growing.
    pub live: bool,
    /// `#EXT-X-MAP`, the initialisation segment fragmented MP4 needs.
    pub init: Option<String>,
    pub encryption: Encryption,
}

impl Media {
    pub fn duration(&self) -> f64 {
        self.segments.iter().map(|segment| segment.duration).sum()
    }

    /// Whether this playlist can be downloaded as a file at all.
    pub fn is_downloadable(&self) -> Result<()> {
        if let Encryption::Unsupported(what) = &self.encryption {
            return Err(MediaError::Unsupported(format!(
                "this stream uses {what}, which Divine IDM cannot decrypt"
            )));
        }
        if self.segments.is_empty() {
            return Err(MediaError::Empty);
        }
        Ok(())
    }
}

/// Parses a master playlist, resolving every URL against `base`.
pub fn parse_master(text: &str, base: &str) -> Result<Master> {
    let base = Url::parse(base).map_err(|_| MediaError::BadUrl(base.to_owned()))?;
    let parsed = m3u8_rs::parse_playlist_res(text.as_bytes())
        .map_err(|error| MediaError::Manifest(error.to_string()))?;

    let m3u8_rs::Playlist::MasterPlaylist(master) = parsed else {
        return Err(MediaError::NotAMaster);
    };

    let variants = master
        .variants
        .iter()
        .map(|variant| {
            Ok(Variant {
                url: resolve(&base, &variant.uri)?,
                bandwidth: variant.bandwidth,
                resolution: variant.resolution.map(|r| (r.width, r.height)),
                codecs: variant.codecs.clone(),
                frame_rate: variant.frame_rate,
                audio_group: variant.audio.clone(),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let renditions = master
        .alternatives
        .iter()
        .map(|alt| {
            Ok(Rendition {
                group_id: alt.group_id.clone(),
                name: alt.name.clone(),
                language: alt.language.clone(),
                url: alt.uri.as_deref().map(|uri| resolve(&base, uri)).transpose()?,
                default: alt.default,
                kind: match alt.media_type {
                    m3u8_rs::AlternativeMediaType::Audio => RenditionKind::Audio,
                    m3u8_rs::AlternativeMediaType::Subtitles => RenditionKind::Subtitles,
                    m3u8_rs::AlternativeMediaType::ClosedCaptions => RenditionKind::ClosedCaptions,
                    _ => RenditionKind::Video,
                },
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(Master { variants, renditions })
}

/// Parses a media playlist, resolving every URL against `base`.
pub fn parse_media(text: &str, base: &str) -> Result<Media> {
    let base_url = Url::parse(base).map_err(|_| MediaError::BadUrl(base.to_owned()))?;
    let parsed = m3u8_rs::parse_playlist_res(text.as_bytes())
        .map_err(|error| MediaError::Manifest(error.to_string()))?;

    let m3u8_rs::Playlist::MediaPlaylist(media) = parsed else {
        return Err(MediaError::NotAMedia);
    };

    // A key applies to every segment after it until another replaces it.
    // Only the first is recorded: a playlist that rotates keys mid-stream
    // needs per-segment handling this does not claim to do, and saying so is
    // better than decrypting most of a file correctly.
    let mut encryption = Encryption::None;
    let mut rotates = false;

    let mut segments = Vec::with_capacity(media.segments.len());
    for (index, segment) in media.segments.iter().enumerate() {
        if let Some(key) = &segment.key {
            let next = classify_key(key, &base_url)?;
            if encryption != Encryption::None && next != encryption {
                rotates = true;
            }
            if encryption == Encryption::None {
                encryption = next;
            }
        }

        segments.push(Segment {
            url: resolve(&base_url, &segment.uri)?,
            duration: segment.duration as f64,
            byte_range: segment
                .byte_range
                .as_ref()
                .and_then(|range| range.offset.map(|offset| (offset, range.length))),
            sequence: media.media_sequence + index as u64,
        });
    }

    if rotates {
        encryption = Encryption::Unsupported("a key that changes mid-stream".to_owned());
    }

    let init = media
        .segments
        .iter()
        .find_map(|segment| segment.map.as_ref())
        .map(|map| resolve(&base_url, &map.uri))
        .transpose()?;

    Ok(Media {
        segments,
        target_duration: media.target_duration as f64,
        live: !media.end_list,
        init,
        encryption,
    })
}

fn classify_key(key: &m3u8_rs::Key, base: &Url) -> Result<Encryption> {
    match key.method {
        m3u8_rs::KeyMethod::None => Ok(Encryption::None),
        m3u8_rs::KeyMethod::AES128 => {
            let Some(uri) = &key.uri else {
                return Ok(Encryption::Unsupported("AES-128 with no key address".to_owned()));
            };
            Ok(Encryption::Aes128 {
                key_url: resolve(base, uri)?,
                iv: key.iv.as_deref().and_then(parse_iv),
            })
        }
        m3u8_rs::KeyMethod::SampleAES => {
            Ok(Encryption::Unsupported("SAMPLE-AES encryption".to_owned()))
        }
        m3u8_rs::KeyMethod::Other(ref other) => Ok(Encryption::Unsupported(other.clone())),
    }
}

/// `0x` followed by 32 hex digits, per the specification.
fn parse_iv(text: &str) -> Option<[u8; 16]> {
    let digits = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X"))?;
    if digits.len() != 32 {
        return None;
    }
    let mut iv = [0u8; 16];
    for (index, byte) in iv.iter_mut().enumerate() {
        *byte = u8::from_str_radix(digits.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(iv)
}

/// Resolves a possibly-relative manifest reference.
///
/// Most segment addresses in the wild are relative, and getting this wrong
/// produces a playlist whose every URL is subtly wrong rather than an error,
/// so it is one function with its own tests.
fn resolve(base: &Url, reference: &str) -> Result<String> {
    base.join(reference.trim())
        .map(|url| url.to_string())
        .map_err(|_| MediaError::BadUrl(reference.to_owned()))
}

fn human_bitrate(bits_per_second: u64) -> String {
    if bits_per_second >= 1_000_000 {
        format!("{:.1} Mbps", bits_per_second as f64 / 1_000_000.0)
    } else {
        format!("{} kbps", bits_per_second / 1_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER: &str = r#"#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="stereo",NAME="English",LANGUAGE="en",DEFAULT=YES,URI="audio/en.m3u8"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="stereo",NAME="French",LANGUAGE="fr",DEFAULT=NO,URI="audio/fr.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,CODECS="avc1.42c01e,mp4a.40.2",AUDIO="stereo"
360/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=2400000,RESOLUTION=1280x720,CODECS="avc1.4d401f",AUDIO="stereo"
720/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=5200000,RESOLUTION=1920x1080,CODECS="avc1.640028",AUDIO="stereo"
1080/index.m3u8
"#;

    const MEDIA: &str = r#"#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:6
#EXT-X-MEDIA-SEQUENCE:0
#EXTINF:6.0,
seg0.ts
#EXTINF:6.0,
seg1.ts
#EXTINF:3.5,
seg2.ts
#EXT-X-ENDLIST
"#;

    #[test]
    fn a_master_playlist_lists_every_rendition() {
        let master = parse_master(MASTER, "https://cdn.example/video/master.m3u8").unwrap();
        assert_eq!(master.variants.len(), 3);
        assert_eq!(master.variants[2].bandwidth, 5_200_000);
        assert_eq!(master.variants[2].resolution, Some((1920, 1080)));
    }

    #[test]
    fn relative_segment_addresses_are_resolved_against_the_playlist() {
        // The common case, and one that fails silently rather than loudly if
        // it is wrong: every URL is plausible and every one is incorrect.
        let master = parse_master(MASTER, "https://cdn.example/video/master.m3u8").unwrap();
        assert_eq!(master.variants[0].url, "https://cdn.example/video/360/index.m3u8");

        let media = parse_media(MEDIA, "https://cdn.example/video/720/index.m3u8").unwrap();
        assert_eq!(media.segments[0].url, "https://cdn.example/video/720/seg0.ts");
    }

    #[test]
    fn an_absolute_segment_address_is_left_alone() {
        let playlist = MEDIA.replace("seg0.ts", "https://other.example/a.ts");
        let media = parse_media(&playlist, "https://cdn.example/video/index.m3u8").unwrap();
        assert_eq!(media.segments[0].url, "https://other.example/a.ts");
    }

    #[test]
    fn the_best_rendition_is_chosen_by_bandwidth_not_height() {
        // Two renditions can share a height and differ sharply in quality;
        // the declared bandwidth is the only thing that separates them.
        let playlist = MASTER.replace("RESOLUTION=1920x1080", "RESOLUTION=1280x720");
        let master = parse_master(&playlist, "https://cdn.example/m.m3u8").unwrap();
        assert_eq!(master.best().unwrap().bandwidth, 5_200_000);
    }

    #[test]
    fn a_height_limit_picks_the_best_that_fits() {
        let master = parse_master(MASTER, "https://cdn.example/m.m3u8").unwrap();
        assert_eq!(master.at_most(720).unwrap().resolution, Some((1280, 720)));
        assert_eq!(master.at_most(360).unwrap().resolution, Some((640, 360)));
    }

    #[test]
    fn asking_for_a_height_no_rendition_offers_still_downloads_something() {
        // Erring towards the smallest rather than towards nothing: an error
        // here is one the user can do nothing about.
        let master = parse_master(MASTER, "https://cdn.example/m.m3u8").unwrap();
        assert_eq!(master.at_most(144).unwrap().resolution, Some((640, 360)));
    }

    #[test]
    fn separate_audio_is_matched_to_the_video_that_wants_it() {
        let master = parse_master(MASTER, "https://cdn.example/video/m.m3u8").unwrap();
        let audio = master.audio_for(master.best().unwrap()).expect("an audio rendition");
        assert_eq!(audio.name, "English", "the default should win");
        assert_eq!(audio.url.as_deref(), Some("https://cdn.example/video/audio/en.m3u8"));
    }

    #[test]
    fn durations_and_sequence_numbers_come_through() {
        let media = parse_media(MEDIA, "https://cdn.example/i.m3u8").unwrap();
        assert_eq!(media.segments.len(), 3);
        assert!((media.duration() - 15.5).abs() < 0.001);
        assert_eq!(media.segments[2].sequence, 2);
        assert!(!media.live);
    }

    #[test]
    fn a_media_sequence_offset_is_honoured() {
        // It is the AES-128 IV when the playlist gives none, so an offset
        // read as zero decrypts every segment into noise.
        let playlist = MEDIA.replace("#EXT-X-MEDIA-SEQUENCE:0", "#EXT-X-MEDIA-SEQUENCE:9000");
        let media = parse_media(&playlist, "https://cdn.example/i.m3u8").unwrap();
        assert_eq!(media.segments[0].sequence, 9000);
        assert_eq!(media.segments[2].sequence, 9002);
    }

    #[test]
    fn a_playlist_without_an_endlist_is_live() {
        let playlist = MEDIA.replace("#EXT-X-ENDLIST\n", "");
        let media = parse_media(&playlist, "https://cdn.example/i.m3u8").unwrap();
        assert!(media.live, "a growing playlist is a live stream");
    }

    #[test]
    fn aes_128_is_recognised_with_its_key_address() {
        let playlist = MEDIA.replace(
            "#EXT-X-MEDIA-SEQUENCE:0",
            "#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x000102030405060708090a0b0c0d0e0f",
        );
        let media = parse_media(&playlist, "https://cdn.example/video/i.m3u8").unwrap();
        match &media.encryption {
            Encryption::Aes128 { key_url, iv } => {
                assert_eq!(key_url, "https://cdn.example/video/key.bin");
                assert_eq!(iv.unwrap()[15], 0x0f);
            }
            other => panic!("expected AES-128, got {other:?}"),
        }
    }

    #[test]
    fn sample_aes_is_refused_by_name_rather_than_half_downloaded() {
        let playlist = MEDIA.replace(
            "#EXT-X-MEDIA-SEQUENCE:0",
            "#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://x\"",
        );
        let media = parse_media(&playlist, "https://cdn.example/i.m3u8").unwrap();
        let error = media.is_downloadable().unwrap_err().to_string();
        assert!(error.contains("SAMPLE-AES"), "{error}");
    }

    #[test]
    fn a_fragmented_mp4_playlist_keeps_its_initialisation_segment() {
        // Without it the segments are unplayable: the moov box lives there.
        let playlist = MEDIA.replace(
            "#EXT-X-MEDIA-SEQUENCE:0",
            "#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-MAP:URI=\"init.mp4\"",
        );
        let media = parse_media(&playlist, "https://cdn.example/video/i.m3u8").unwrap();
        assert_eq!(media.init.as_deref(), Some("https://cdn.example/video/init.mp4"));
    }

    #[test]
    fn a_master_playlist_given_where_a_media_one_is_expected_is_an_error() {
        assert!(matches!(
            parse_media(MASTER, "https://cdn.example/m.m3u8"),
            Err(MediaError::NotAMedia)
        ));
        assert!(matches!(
            parse_master(MEDIA, "https://cdn.example/i.m3u8"),
            Err(MediaError::NotAMaster)
        ));
    }

    #[test]
    fn an_empty_playlist_is_refused() {
        let playlist = "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-ENDLIST\n";
        let media = parse_media(playlist, "https://cdn.example/i.m3u8").unwrap();
        assert!(matches!(media.is_downloadable(), Err(MediaError::Empty)));
    }

    #[test]
    fn nonsense_is_an_error_not_a_panic() {
        // Manifests are fetched from whoever is serving the page.
        for text in ["", "not a playlist", "#EXTM3U\n\u{0}\u{1}", "#EXTM3U\n#EXTINF:abc,\nx.ts"] {
            let _ = parse_master(text, "https://cdn.example/m.m3u8");
            let _ = parse_media(text, "https://cdn.example/i.m3u8");
        }
    }

    #[test]
    fn labels_read_the_way_a_picker_needs() {
        let master = parse_master(MASTER, "https://cdn.example/m.m3u8").unwrap();
        assert_eq!(master.best().unwrap().label(), "1080p · 5.2 Mbps");
    }
}
