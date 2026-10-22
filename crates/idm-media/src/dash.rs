//! DASH manifests, as far as a downloader needs them.
//!
//! The specification is enormous and most of it describes playback: timing,
//! switching, ad insertion, live edges. A downloader needs one question
//! answered — which addresses make up the rendition I picked — so this reads
//! the three ways a manifest can answer it and says plainly when it meets a
//! fourth.
//!
//! Parsed with `quick-xml` rather than a DASH crate. The crates that handle
//! the whole specification bring their own HTTP stack and async runtime,
//! which would put a second way of fetching things into a project whose
//! point is the first one.

use quick_xml::Reader;
use quick_xml::events::Event;
use url::Url;

use crate::{MediaError, Result};

#[derive(Debug, Clone, PartialEq)]
pub struct Representation {
    pub id: String,
    pub bandwidth: u64,
    pub width: Option<u64>,
    pub height: Option<u64>,
    pub codecs: Option<String>,
    pub mime: String,
    pub init: Option<String>,
    pub segments: Vec<String>,
}

impl Representation {
    pub fn is_video(&self) -> bool {
        self.mime.starts_with("video/")
            || self.height.is_some()
            || self.codecs.as_deref().is_some_and(|c| c.starts_with("avc") || c.starts_with("hev"))
    }

    pub fn is_audio(&self) -> bool {
        self.mime.starts_with("audio/") || (!self.is_video() && self.codecs.is_some())
    }

    pub fn label(&self) -> String {
        match self.height {
            Some(height) => format!("{height}p · {:.1} Mbps", self.bandwidth as f64 / 1e6),
            None => format!("audio · {} kbps", self.bandwidth / 1000),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Manifest {
    pub representations: Vec<Representation>,
    pub live: bool,
}

impl Manifest {
    pub fn best_video(&self) -> Option<&Representation> {
        self.representations.iter().filter(|r| r.is_video()).max_by_key(|r| r.bandwidth)
    }

    pub fn best_audio(&self) -> Option<&Representation> {
        self.representations
            .iter()
            .filter(|r| r.is_audio() && !r.is_video())
            .max_by_key(|r| r.bandwidth)
    }

    pub fn at_most(&self, height: u64) -> Option<&Representation> {
        self.representations
            .iter()
            .filter(|r| r.is_video() && r.height.is_some_and(|h| h <= height))
            .max_by_key(|r| r.bandwidth)
            .or_else(|| {
                self.representations.iter().filter(|r| r.is_video()).min_by_key(|r| r.bandwidth)
            })
    }
}

/// Everything inherited from an enclosing element.
///
/// DASH lets `SegmentTemplate`, `BaseURL` and the descriptive attributes sit
/// on the Period, the AdaptationSet or the Representation, with the inner
/// one winning. Carrying them down explicitly is what makes that work.
#[derive(Debug, Clone, Default)]
struct Inherited {
    base: Option<String>,
    template: Option<Template>,
    segment_list: Vec<String>,
    list_duration: Option<u64>,
    mime: Option<String>,
    codecs: Option<String>,
    width: Option<u64>,
    height: Option<u64>,
}

#[derive(Debug, Clone, Default)]
struct Template {
    media: Option<String>,
    initialisation: Option<String>,
    start_number: u64,
    /// Summed from `SegmentTimeline`, when there is one.
    timeline_count: Option<u64>,
    duration: Option<u64>,
    timescale: u64,
}

/// Elements that own a scope.
///
/// DASH lets `SegmentTemplate`, `SegmentList` and `BaseURL` sit at any of
/// these levels, with the innermost winning. Only these push a frame;
/// everything else modifies the frame of whatever contains it. Giving every
/// element its own frame is the mistake that made this read nothing at all —
/// a `SegmentTemplate` recorded against itself is discarded the moment it
/// closes, and the representations that should have inherited it see an
/// empty scope.
fn owns_scope(name: &str) -> bool {
    matches!(name, "MPD" | "Period" | "AdaptationSet" | "Representation")
}

/// Reads a manifest, resolving every address against `base`.
pub fn parse(text: &str, base: &str) -> Result<Manifest> {
    let base_url = Url::parse(base).map_err(|_| MediaError::BadUrl(base.to_owned()))?;

    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);

    let mut manifest = Manifest::default();
    let mut scopes: Vec<Inherited> = vec![Inherited::default()];
    let mut pending: Vec<Option<Representation>> = Vec::new();
    let mut reading_base_url = false;
    let mut saw_mpd = false;
    let mut duration_seconds = 0.0f64;

    loop {
        let event = match reader.read_event() {
            Err(error) => return Err(MediaError::Manifest(error.to_string())),
            Ok(Event::Eof) => break,
            Ok(event) => event,
        };

        match event {
            Event::Start(ref element) | Event::Empty(ref element) => {
                // A self-closing element never produces an End, so it has to
                // be opened and closed here. Manifests write
                // `<Representation .../>` far more often than not.
                let self_closing = matches!(event, Event::Empty(_));
                let name = local_name(element.name().as_ref());

                if owns_scope(&name) {
                    let inherited = scopes.last().cloned().unwrap_or_default();
                    scopes.push(inherited);
                }
                let scope = scopes.last_mut().expect("a scope is always open");

                match name.as_str() {
                    "MPD" => {
                        saw_mpd = true;
                        manifest.live = attribute(element, "type")
                            .is_some_and(|t| t.eq_ignore_ascii_case("dynamic"));
                        if let Some(iso) = attribute(element, "mediaPresentationDuration") {
                            duration_seconds = parse_iso_duration(&iso);
                        }
                    }
                    "BaseURL" => reading_base_url = true,
                    "SegmentTemplate" => {
                        // Replaces rather than merges: an inner template is a
                        // complete answer, and merging it with an outer one
                        // would mix two addressing schemes.
                        scope.template = Some(Template {
                            media: attribute(element, "media"),
                            initialisation: attribute(element, "initialization")
                                .or_else(|| attribute(element, "initialisation")),
                            start_number: number(element, "startNumber").unwrap_or(1),
                            timeline_count: None,
                            duration: number(element, "duration"),
                            timescale: number(element, "timescale").unwrap_or(1),
                        });
                    }
                    "SegmentList" => {
                        scope.segment_list.clear();
                        scope.list_duration = number(element, "duration");
                    }
                    "S" => {
                        // One timeline entry covers `r + 1` segments.
                        let repeat = number(element, "r").unwrap_or(0);
                        if let Some(template) = scope.template.as_mut() {
                            *template.timeline_count.get_or_insert(0) += repeat + 1;
                        }
                    }
                    "SegmentURL" => {
                        if let Some(media) = attribute(element, "media") {
                            scope.segment_list.push(media);
                        }
                    }
                    "Initialization" | "Initialisation" => {
                        if let Some(source) = attribute(element, "sourceURL") {
                            scope.template.get_or_insert_with(Default::default).initialisation =
                                Some(source);
                        }
                    }
                    "AdaptationSet" => {
                        scope.mime = attribute(element, "mimeType").or(scope.mime.take());
                        scope.codecs = attribute(element, "codecs").or(scope.codecs.take());
                        scope.width = number(element, "width").or(scope.width);
                        scope.height = number(element, "height").or(scope.height);
                    }
                    "Representation" => {
                        scope.mime = attribute(element, "mimeType").or(scope.mime.take());
                        scope.codecs = attribute(element, "codecs").or(scope.codecs.take());
                        scope.width = number(element, "width").or(scope.width);
                        scope.height = number(element, "height").or(scope.height);

                        pending.push(Some(Representation {
                            id: attribute(element, "id").unwrap_or_default(),
                            bandwidth: number(element, "bandwidth").unwrap_or(0),
                            width: scope.width,
                            height: scope.height,
                            codecs: scope.codecs.clone(),
                            mime: scope.mime.clone().unwrap_or_default(),
                            init: None,
                            segments: Vec::new(),
                        }));
                    }
                    _ => {}
                }

                if self_closing {
                    if name == "BaseURL" {
                        reading_base_url = false;
                    }
                    if owns_scope(&name) {
                        close_scope(
                            &name,
                            &mut scopes,
                            &mut pending,
                            &mut manifest,
                            &base_url,
                            duration_seconds,
                        )?;
                    }
                }
            }

            Event::Text(ref body) if reading_base_url => {
                let value = body.decode().unwrap_or_default().trim().to_owned();
                if !value.is_empty()
                    && let Some(scope) = scopes.last_mut()
                {
                    scope.base = Some(value);
                }
            }

            Event::End(ref element) => {
                let name = local_name(element.name().as_ref());
                if name == "BaseURL" {
                    reading_base_url = false;
                }
                if owns_scope(&name) {
                    close_scope(
                        &name,
                        &mut scopes,
                        &mut pending,
                        &mut manifest,
                        &base_url,
                        duration_seconds,
                    )?;
                }
            }
            _ => {}
        }
    }

    if !saw_mpd {
        return Err(MediaError::Manifest("not a DASH manifest".to_owned()));
    }
    if manifest.representations.is_empty() {
        return Err(MediaError::Empty);
    }
    Ok(manifest)
}

/// Pops a scope, finishing the representation it described.
fn close_scope(
    name: &str,
    scopes: &mut Vec<Inherited>,
    pending: &mut Vec<Option<Representation>>,
    manifest: &mut Manifest,
    base: &Url,
    duration_seconds: f64,
) -> Result<()> {
    let scope = scopes.pop().unwrap_or_default();
    if name != "Representation" {
        return Ok(());
    }

    let Some(Some(mut representation)) = pending.pop() else {
        return Ok(());
    };
    fill_addresses(&mut representation, &scope, base, duration_seconds)?;
    if !representation.segments.is_empty() {
        manifest.representations.push(representation);
    }
    Ok(())
}

/// Works out the addresses for one rendition, by whichever of the three
/// mechanisms the manifest used.
fn fill_addresses(
    representation: &mut Representation,
    frame: &Inherited,
    base: &Url,
    total_duration_seconds: f64,
) -> Result<()> {
    let root = match &frame.base {
        Some(relative) => base.join(relative).map_err(|_| MediaError::BadUrl(relative.clone()))?,
        None => base.clone(),
    };

    // 1. SegmentTemplate: addresses are generated from a pattern.
    if let Some(template) = &frame.template
        && let Some(pattern) = &template.media
    {
        let count = segment_count(template, total_duration_seconds);
        if let Some(initialisation) = &template.initialisation {
            representation.init = Some(resolve(&root, &expand(initialisation, representation, 0))?);
        }
        for index in 0..count {
            let number = template.start_number + index;
            representation.segments.push(resolve(&root, &expand(pattern, representation, number))?);
        }
        return Ok(());
    }

    // 2. SegmentList: addresses are given one by one.
    if !frame.segment_list.is_empty() {
        if let Some(template) = &frame.template
            && let Some(initialisation) = &template.initialisation
        {
            representation.init = Some(resolve(&root, initialisation)?);
        }
        for media in &frame.segment_list {
            representation.segments.push(resolve(&root, media)?);
        }
        return Ok(());
    }

    // 3. A plain BaseURL: the whole rendition is one file. This is the case
    //    our own scheduler handles best, since it is an ordinary ranged
    //    download rather than hundreds of small ones.
    if frame.base.is_some() {
        representation.segments.push(root.to_string());
    }
    Ok(())
}

/// How many segments a template describes.
///
/// A `SegmentTimeline` states it. Otherwise it has to be derived from the
/// presentation duration and the segment duration, and a manifest that gives
/// neither leaves no honest answer — zero, so the rendition is dropped
/// rather than silently truncated to a guess.
fn segment_count(template: &Template, total_duration_seconds: f64) -> u64 {
    if let Some(count) = template.timeline_count {
        return count;
    }
    let (Some(duration), true) = (template.duration, total_duration_seconds > 0.0) else {
        return 0;
    };
    if duration == 0 {
        return 0;
    }
    let seconds_per_segment = duration as f64 / template.timescale.max(1) as f64;
    (total_duration_seconds / seconds_per_segment).ceil() as u64
}

/// Substitutes the `$...$` placeholders a template uses.
fn expand(pattern: &str, representation: &Representation, number: u64) -> String {
    let mut out = pattern.replace("$RepresentationID$", &representation.id);
    out = out.replace("$Bandwidth$", &representation.bandwidth.to_string());
    // `$Number%05d$` pads; the width is what follows the percent sign.
    while let Some(start) = out.find("$Number%") {
        let Some(end) = out[start..].find('$').and_then(|_| out[start + 1..].find('$')) else {
            break;
        };
        let token = &out[start..start + end + 2];
        let width: usize =
            token.trim_start_matches("$Number%").trim_end_matches(['d', '$']).parse().unwrap_or(1);
        out = out.replacen(token, &format!("{number:0width$}"), 1);
    }
    out.replace("$Number$", &number.to_string()).replace("$$", "$")
}

fn resolve(base: &Url, reference: &str) -> Result<String> {
    base.join(reference.trim())
        .map(|url| url.to_string())
        .map_err(|_| MediaError::BadUrl(reference.to_owned()))
}

fn local_name(raw: &[u8]) -> String {
    let name = String::from_utf8_lossy(raw);
    name.rsplit(':').next().unwrap_or(&name).to_owned()
}

fn attribute(element: &quick_xml::events::BytesStart<'_>, name: &str) -> Option<String> {
    element.attributes().flatten().find_map(|attribute| {
        (local_name(attribute.key.as_ref()) == name)
            .then(|| String::from_utf8_lossy(&attribute.value).into_owned())
    })
}

fn number(element: &quick_xml::events::BytesStart<'_>, name: &str) -> Option<u64> {
    attribute(element, name)?.parse().ok()
}

/// `PT1H2M3.5S` and friends.
fn parse_iso_duration(text: &str) -> f64 {
    let Some(rest) = text.strip_prefix('P') else {
        return 0.0;
    };
    let Some((_, time)) = rest.split_once('T') else {
        return 0.0;
    };

    let mut seconds = 0.0;
    let mut digits = String::new();
    for character in time.chars() {
        if character.is_ascii_digit() || character == '.' {
            digits.push(character);
            continue;
        }
        let value: f64 = digits.parse().unwrap_or(0.0);
        digits.clear();
        seconds += match character {
            'H' => value * 3600.0,
            'M' => value * 60.0,
            'S' => value,
            _ => 0.0,
        };
    }
    seconds
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEMPLATE_MPD: &str = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT1M0S">
  <Period>
    <AdaptationSet mimeType="video/mp4" codecs="avc1.640028">
      <SegmentTemplate media="$RepresentationID$/seg-$Number$.m4s"
                       initialization="$RepresentationID$/init.mp4"
                       startNumber="1" duration="2" timescale="1"/>
      <Representation id="v720" bandwidth="2400000" width="1280" height="720"/>
      <Representation id="v1080" bandwidth="5000000" width="1920" height="1080"/>
    </AdaptationSet>
    <AdaptationSet mimeType="audio/mp4" codecs="mp4a.40.2">
      <SegmentTemplate media="$RepresentationID$/a-$Number$.m4s"
                       initialization="$RepresentationID$/init.mp4"
                       startNumber="1" duration="2" timescale="1"/>
      <Representation id="a128" bandwidth="128000"/>
    </AdaptationSet>
  </Period>
</MPD>
"#;

    #[test]
    fn a_template_generates_every_segment_address() {
        let manifest = parse(TEMPLATE_MPD, "https://cdn.example/dash/manifest.mpd").unwrap();
        let video = manifest.best_video().unwrap();
        assert_eq!(video.id, "v1080");
        // Sixty seconds of two-second segments.
        assert_eq!(video.segments.len(), 30);
        assert_eq!(video.segments[0], "https://cdn.example/dash/v1080/seg-1.m4s");
        assert_eq!(video.segments[29], "https://cdn.example/dash/v1080/seg-30.m4s");
        assert_eq!(video.init.as_deref(), Some("https://cdn.example/dash/v1080/init.mp4"));
    }

    #[test]
    fn audio_and_video_are_told_apart() {
        let manifest = parse(TEMPLATE_MPD, "https://cdn.example/dash/m.mpd").unwrap();
        assert_eq!(manifest.best_video().unwrap().height, Some(1080));
        assert_eq!(manifest.best_audio().unwrap().id, "a128");
        assert!(manifest.best_audio().unwrap().segments.len() == 30);
    }

    #[test]
    fn attributes_are_inherited_from_the_adaptation_set() {
        // The template and the codecs sit on the enclosing element; a
        // representation that did not inherit them would have no addresses.
        let manifest = parse(TEMPLATE_MPD, "https://cdn.example/dash/m.mpd").unwrap();
        let video = manifest.best_video().unwrap();
        assert_eq!(video.mime, "video/mp4");
        assert_eq!(video.codecs.as_deref(), Some("avc1.640028"));
    }

    #[test]
    fn a_height_limit_picks_the_best_that_fits() {
        let manifest = parse(TEMPLATE_MPD, "https://cdn.example/dash/m.mpd").unwrap();
        assert_eq!(manifest.at_most(720).unwrap().id, "v720");
    }

    #[test]
    fn a_padded_number_keeps_its_width() {
        // $Number%05d$ is common, and a server that wanted seg-00007 will
        // not serve seg-7.
        let mpd = TEMPLATE_MPD.replace("seg-$Number$", "seg-$Number%05d$");
        let manifest = parse(&mpd, "https://cdn.example/dash/m.mpd").unwrap();
        assert_eq!(
            manifest.best_video().unwrap().segments[6],
            "https://cdn.example/dash/v1080/seg-00007.m4s"
        );
    }

    #[test]
    fn a_segment_timeline_states_the_count_itself() {
        let mpd = r#"<?xml version="1.0"?>
<MPD type="static">
  <Period>
    <AdaptationSet mimeType="video/mp4">
      <SegmentTemplate media="v/$Number$.m4s" initialization="v/init.mp4" startNumber="1">
        <SegmentTimeline>
          <S t="0" d="90000" r="4"/>
          <S d="90000"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="v" bandwidth="1000000" width="1280" height="720"/>
    </AdaptationSet>
  </Period>
</MPD>
"#;
        let manifest = parse(mpd, "https://cdn.example/d/m.mpd").unwrap();
        // r="4" means five, plus one more.
        assert_eq!(manifest.best_video().unwrap().segments.len(), 6);
    }

    #[test]
    fn a_segment_list_is_taken_literally() {
        let mpd = r#"<?xml version="1.0"?>
<MPD type="static">
  <Period>
    <AdaptationSet mimeType="video/mp4">
      <Representation id="v" bandwidth="1000000" width="640" height="360">
        <SegmentList>
          <Initialization sourceURL="init.mp4"/>
          <SegmentURL media="a.m4s"/>
          <SegmentURL media="b.m4s"/>
        </SegmentList>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>
"#;
        let manifest = parse(mpd, "https://cdn.example/d/m.mpd").unwrap();
        let video = &manifest.representations[0];
        assert_eq!(video.segments.len(), 2);
        assert_eq!(video.segments[1], "https://cdn.example/d/b.m4s");
        assert_eq!(video.init.as_deref(), Some("https://cdn.example/d/init.mp4"));
    }

    #[test]
    fn a_single_file_rendition_is_one_ordinary_download() {
        // The case our own scheduler is best at: one ranged request rather
        // than hundreds of small ones.
        let mpd = r#"<?xml version="1.0"?>
<MPD type="static">
  <Period>
    <AdaptationSet mimeType="video/mp4">
      <Representation id="v" bandwidth="1000000" width="1920" height="1080">
        <BaseURL>movie.mp4</BaseURL>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>
"#;
        let manifest = parse(mpd, "https://cdn.example/d/m.mpd").unwrap();
        assert_eq!(manifest.representations[0].segments, vec!["https://cdn.example/d/movie.mp4"]);
    }

    #[test]
    fn a_live_manifest_says_so() {
        let mpd = TEMPLATE_MPD.replace(r#"type="static""#, r#"type="dynamic""#);
        assert!(parse(&mpd, "https://cdn.example/d/m.mpd").unwrap().live);
    }

    #[test]
    fn namespaced_elements_are_read_the_same() {
        // Plenty of real manifests prefix everything.
        let mpd = TEMPLATE_MPD
            .replace("<MPD ", "<mpd:MPD ")
            .replace("</MPD>", "</mpd:MPD>")
            .replace("<Period>", "<mpd:Period>")
            .replace("</Period>", "</mpd:Period>")
            .replace("<AdaptationSet", "<mpd:AdaptationSet")
            .replace("</AdaptationSet>", "</mpd:AdaptationSet>")
            .replace("<SegmentTemplate", "<mpd:SegmentTemplate")
            .replace("<Representation", "<mpd:Representation");
        let manifest = parse(&mpd, "https://cdn.example/d/m.mpd").unwrap();
        assert_eq!(manifest.representations.len(), 3);
    }

    #[test]
    fn iso_durations_are_read() {
        assert!((parse_iso_duration("PT1H2M3.5S") - 3723.5).abs() < 0.001);
        assert!((parse_iso_duration("PT30S") - 30.0).abs() < 0.001);
        assert_eq!(parse_iso_duration("nonsense"), 0.0);
    }

    #[test]
    fn a_manifest_that_states_no_count_is_dropped_rather_than_guessed() {
        // No timeline and no presentation duration: any number of segments
        // would be invention, and a file missing its end is worse than a
        // refusal.
        let mpd = r#"<?xml version="1.0"?>
<MPD type="static">
  <Period>
    <AdaptationSet mimeType="video/mp4">
      <SegmentTemplate media="v/$Number$.m4s" startNumber="1"/>
      <Representation id="v" bandwidth="1" width="640" height="360"/>
    </AdaptationSet>
  </Period>
</MPD>
"#;
        assert!(matches!(parse(mpd, "https://cdn.example/d/m.mpd"), Err(MediaError::Empty)));
    }

    #[test]
    fn nonsense_is_an_error_not_a_panic() {
        for text in ["", "<html></html>", "<MPD", "<?xml version=\"1.0\"?><MPD/>"] {
            let _ = parse(text, "https://cdn.example/d/m.mpd");
        }
    }
}
