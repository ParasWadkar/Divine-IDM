//! Collecting the links off a page, so a list can be downloaded at once.
//!
//! The thing people actually want from a "site grabber" is narrow: a page
//! of episodes, a directory listing, a release page with twelve mirrors —
//! and a way to tick the ones they meant rather than pasting twelve
//! addresses. That is what this does.
//!
//! # Why this is not a crawler
//!
//! "Crawl a page" is one careless parameter away from "crawl the
//! internet", and the careless parameter is always depth. So there is no
//! depth number here. [`Depth`] has two values — this page, or this page
//! and the pages it links to — and the second one does **not** recurse:
//! links found on a followed page are collected, never followed. There is
//! no arithmetic anywhere that could be made to mean three.
//!
//! Everything else is bounded too, by named constants rather than by
//! hoping:
//!
//! - [`MOST_PAGES`] pages are ever fetched.
//! - [`MOST_LINKS`] links are ever returned.
//! - [`MOST_BODY_BYTES`] of any one page is read, and the cap is checked
//!   while the body arrives rather than after it.
//! - [`LONGEST_GRAB`] bounds the whole thing in wall-clock time, which is
//!   the only cap that still holds when every page is slow rather than
//!   large.
//! - Only the origin the start page came from is ever fetched. Compared
//!   after redirects, because an origin check on the address that was
//!   *asked for* is escaped by a single `302`.
//! - Requests go one at a time. A grab of twenty-five pages should not
//!   look like a small attack on somebody's blog.
//!
//! And `robots.txt` is fetched and honoured. Not because it has any force
//! — it does not — but because a program that fetches pages on its own
//! initiative is the kind of program it was written for, and ignoring it
//! while claiming to be a download manager is how a download manager gets
//! an IP range banned on its users' behalf.
//!
//! # Where the parsing comes from
//!
//! [`lol_html`] rather than a DOM parser, because the handlers are fed
//! chunks and keep nothing: a ten-megabyte page costs the links found in
//! it and not a tree of it. The parser is also given a hard memory
//! ceiling, so a page built to make a parser allocate gets a short answer
//! instead.

use std::cell::RefCell;
use std::collections::HashSet;
use std::time::Duration;

use idm_net::Url;
use idm_net::client::HttpConfig;
use lol_html::html_content::Element;
use lol_html::{HtmlRewriter, MemorySettings, Settings, element, text};

/// How far a grab goes.
///
/// Two values, never a number. See the module docs: a depth integer is the
/// one parameter that turns this into a crawler, so there isn't one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    /// Only the page that was named.
    ThisPage,
    /// That page, and the pages it links to — whose links are collected
    /// but whose links are *not* followed.
    AndWhatItLinksTo,
}

/// How many pages are ever fetched, including the first.
pub const MOST_PAGES: usize = 25;

/// How many links are ever returned.
///
/// Five hundred is already more than anybody will read; the point of the
/// cap is that a page listing a hundred thousand links costs the first
/// five hundred of them and then nothing.
pub const MOST_LINKS: usize = 500;

/// How much of any one page is read.
pub const MOST_BODY_BYTES: usize = 2 << 20;

/// How much of a `robots.txt` is read.
///
/// They are small, and one that is not is not a `robots.txt`.
pub const MOST_ROBOTS_BYTES: usize = 64 << 10;

/// How long the whole grab may take.
///
/// The cap the others cannot replace: twenty-five pages that each take a
/// minute are within every byte and page limit here and still a two-minute
/// wait with nothing on screen.
pub const LONGEST_GRAB: Duration = Duration::from_secs(45);

/// How long the parser may hold at once.
pub const MOST_PARSER_BYTES: usize = 4 << 20;

/// The longest `Crawl-delay` that will be waited out.
///
/// Some sites ask for a minute, which here would mean the grab achieving
/// nothing inside [`LONGEST_GRAB`]. Honoured up to this and then capped —
/// stated plainly rather than silently ignored.
pub const LONGEST_CRAWL_DELAY: Duration = Duration::from_secs(5);

/// What this calls itself in a `robots.txt` group.
pub const ROBOT_NAME: &str = "divinepulse";

/// One link that was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// Absolute, resolved against the page it was on, fragment removed.
    pub url: String,
    /// The anchor's own text, or an image's `alt` — trimmed, collapsed and
    /// cut short. For showing beside the address in a picker, nothing more.
    pub text: String,
    /// The page it was found on.
    pub from: String,
    /// Whether it came from something that navigates — an `<a>` or an
    /// `<area>` — as opposed to a `src` on an image or a video.
    ///
    /// Only these are candidates for following, which is a different
    /// question from whether they are worth downloading.
    pub navigable: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum GrabError {
    #[error("{0}")]
    Address(String),
    #[error("{0}")]
    Fetch(String),
    #[error("that address is not a web page ({0}), so there are no links on it to collect")]
    NotAPage(String),
    #[error("that site's robots.txt asks programs not to fetch this page")]
    Robots,
}

/// What a grab came back with.
#[derive(Debug, Clone)]
pub struct Grab {
    pub links: Vec<Found>,
    /// The pages that were read, in the order they were read.
    pub pages: Vec<String>,
    /// Why it stopped short, when it did. `None` means it finished.
    ///
    /// Said rather than hidden: a list that quietly stops at five hundred
    /// looks like a complete list of five hundred things.
    pub note: Option<String>,
}

/// Collects the links on `start`, and optionally on the pages it links to.
pub async fn grab(config: &HttpConfig, start: &str, depth: Depth) -> Result<Grab, GrabError> {
    let origin = Url::parse(start).map_err(|error| GrabError::Address(error.to_string()))?;
    if !matches!(origin.scheme(), "http" | "https") {
        return Err(GrabError::Address("only http and https pages can be read for links".into()));
    }

    let robots = robots_for(config, &origin).await;
    if !robots.allows(origin.path()) {
        return Err(GrabError::Robots);
    }

    let began = tokio::time::Instant::now();
    let mut queue = std::collections::VecDeque::from([start.to_owned()]);
    let mut asked: HashSet<String> = HashSet::from([start.to_owned()]);
    let mut seen: HashSet<String> = HashSet::new();
    let mut grab = Grab { links: Vec::new(), pages: Vec::new(), note: None };
    let mut first = true;

    while let Some(next) = queue.pop_front() {
        if grab.pages.len() >= MOST_PAGES {
            grab.note = Some(format!("stopped after reading {MOST_PAGES} pages"));
            break;
        }
        if began.elapsed() >= LONGEST_GRAB {
            grab.note = Some(format!(
                "stopped after {} seconds, having read {} page{}",
                LONGEST_GRAB.as_secs(),
                grab.pages.len(),
                if grab.pages.len() == 1 { "" } else { "s" }
            ));
            break;
        }

        // One at a time, and with whatever wait the site asked for between
        // them. Twenty-five pages at once is a small flood.
        if !first {
            tokio::time::sleep(robots.delay.min(LONGEST_CRAWL_DELAY)).await;
        }

        let page = match idm_net::range::read_page(config, &next, MOST_BODY_BYTES).await {
            Ok(page) => page,
            Err(error) if first => return Err(GrabError::Fetch(error.to_string())),
            // A followed page that will not load is not a failure of the
            // grab. The start page was the thing that was asked for.
            Err(_) => continue,
        };

        let kind = page.content_type.clone().unwrap_or_default();
        if !is_html(&kind) {
            if first {
                let said = if kind.is_empty() { "it did not say what".to_owned() } else { kind };
                return Err(GrabError::NotAPage(said));
            }
            continue;
        }
        if page.status >= 400 {
            if first {
                return Err(GrabError::Fetch(format!("the server answered {}", page.status)));
            }
            continue;
        }

        // The address the body arrived from, which is what its relative
        // links resolve against and what the origin rule is applied to.
        let landed = Url::parse(&page.url).unwrap_or_else(|_| origin.clone());
        let body = String::from_utf8_lossy(&page.bytes);
        let found = links_in(&body, &landed);
        grab.pages.push(page.url.clone());
        if found.truncated {
            grab.note = Some(format!("that page holds more than {MOST_LINKS} links"));
        }

        let follow = first && depth == Depth::AndWhatItLinksTo;
        first = false;

        for link in found.found {
            if follow
                && link.navigable
                && queue.len() + grab.pages.len() < MOST_PAGES
                && let Ok(candidate) = Url::parse(&link.url)
                && same_origin(&origin, &candidate)
                && looks_like_a_page(&candidate)
                && robots.allows(candidate.path())
                && asked.insert(link.url.clone())
            {
                queue.push_back(link.url.clone());
            }

            if !seen.insert(link.url.clone()) {
                continue;
            }
            if grab.links.len() >= MOST_LINKS {
                grab.note = Some(format!("stopped after finding {MOST_LINKS} links"));
                return Ok(grab);
            }
            grab.links.push(link);
        }
    }

    Ok(grab)
}

/// Whether a `Content-Type` describes something with links in it.
///
/// Checked rather than assumed, because the alternative is handing a
/// hundred megabytes of video to an HTML parser on the strength of the
/// address ending in `.html`.
fn is_html(content_type: &str) -> bool {
    let kind = content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    kind == "text/html" || kind == "application/xhtml+xml"
}

/// Whether an address is worth fetching *as a page*.
///
/// By extension, and deliberately generous: anything this does not
/// recognise as a file is treated as a page, because the common case is a
/// path with no extension at all. The content type is checked again after
/// fetching, so a wrong guess here costs one request and no parsing.
fn looks_like_a_page(url: &Url) -> bool {
    let last = url.path_segments().and_then(|mut parts| parts.next_back()).unwrap_or("");
    crate::category::category_for(last).is_none()
}

/// Whether two addresses are the same origin: scheme, host and port.
///
/// Port compared after defaults are filled in, so `https://a/` and
/// `https://a:443/` are one origin rather than two.
pub fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

// ---------------------------------------------------------------------------
// Finding the links
// ---------------------------------------------------------------------------

/// A link as the page wrote it, before resolution.
struct Raw {
    value: String,
    text: String,
    navigable: bool,
}

/// What one page gave up.
#[derive(Debug, Clone, Default)]
pub struct Links {
    pub found: Vec<Found>,
    /// True when the page held more than [`MOST_LINKS`] and the rest were
    /// dropped. Reported rather than swallowed: a list that silently stops
    /// at five hundred looks like a complete list of five hundred things.
    pub truncated: bool,
}

/// Pulls the links out of a page.
///
/// Pure: a string in, links out, no clock and no socket, which is what
/// makes the awkward parts — a `<base href>`, a protocol-relative address,
/// an anchor wrapped round an image — testable as cases rather than as
/// behaviour observed against some live site.
pub fn links_in(html: &str, from: &Url) -> Links {
    let raw: RefCell<Vec<Raw>> = RefCell::new(Vec::new());
    let base: RefCell<Option<String>> = RefCell::new(None);
    // Which entry the text inside an anchor belongs to. An `<img>` nested
    // in that anchor pushes an entry of its own in between, so "the last
    // one" is the wrong answer.
    let anchor: RefCell<Option<usize>> = RefCell::new(None);

    let settings = Settings::new()
        .with_memory_settings(
            MemorySettings::new().with_max_allowed_memory_usage(MOST_PARSER_BYTES),
        )
        // Not strict: this is reading somebody else's HTML, which is
        // allowed to be a mess. Strict mode turns an ambiguous parse into
        // an error, and an error here would mean no links at all.
        .with_strict(false)
        .append_element_content_handler(element!("base[href]", |el: &mut Element| {
            // The first one wins, as a browser does.
            if base.borrow().is_none() {
                *base.borrow_mut() = el.get_attribute("href");
            }
            Ok(())
        }))
        .append_element_content_handler(element!("a, area", |el: &mut Element| {
            match el.get_attribute("href") {
                Some(href) => {
                    let mut raw = raw.borrow_mut();
                    *anchor.borrow_mut() = Some(raw.len());
                    raw.push(Raw { value: href, text: String::new(), navigable: true });
                }
                // An anchor with no href is a name, not a link -- and its
                // text must not be filed under the previous one.
                None => *anchor.borrow_mut() = None,
            }
            Ok(())
        }))
        .append_element_content_handler(text!("a", |chunk| {
            if let Some(at) = *anchor.borrow() {
                let mut raw = raw.borrow_mut();
                if let Some(entry) = raw.get_mut(at)
                    && entry.text.len() < LONGEST_TEXT
                {
                    entry.text.push_str(chunk.as_str());
                }
            }
            Ok(())
        }))
        .append_element_content_handler(element!(
            "img[src], source[src], video[src], audio[src], embed[src]",
            |el: &mut Element| {
                if let Some(src) = el.get_attribute("src") {
                    let text = el.get_attribute("alt").or_else(|| el.get_attribute("title"));
                    raw.borrow_mut().push(Raw {
                        value: src,
                        text: text.unwrap_or_default(),
                        navigable: false,
                    });
                }
                Ok(())
            }
        ));

    {
        // Nothing is written out: this is a read, and the sink exists only
        // because the rewriter is a rewriter.
        let mut rewriter = HtmlRewriter::new(settings, |_: &[u8]| {});
        // A parse that gives up -- a page past the memory ceiling, or one
        // the handlers could not be run over -- keeps whatever was found
        // before it did. Half a list beats none, and the caller is told
        // nothing was wrong because, for its purposes, nothing was.
        if rewriter.write(html.as_bytes()).is_ok() {
            let _ = rewriter.end();
        }
    }

    let against = base
        .into_inner()
        .and_then(|href| from.join(&href).ok())
        .filter(|base| matches!(base.scheme(), "http" | "https"))
        .unwrap_or_else(|| from.clone());

    let mut seen = HashSet::new();
    let mut out = Links::default();
    for entry in raw.into_inner() {
        let value = entry.value.trim();
        if value.is_empty() || value.starts_with('#') {
            continue;
        }
        let Ok(mut url) = against.join(value) else {
            continue;
        };
        if !matches!(url.scheme(), "http" | "https") {
            // `javascript:`, `mailto:`, `data:` and whatever else a page
            // puts in an href. None of them is a download.
            continue;
        }
        // A fragment names a place in a page, never a different file, so
        // keeping it would list the same address several times.
        url.set_fragment(None);
        let url = url.into();

        if !seen.insert(String::clone(&url)) {
            continue;
        }
        if out.found.len() >= MOST_LINKS {
            out.truncated = true;
            break;
        }
        out.found.push(Found {
            url,
            text: tidy(&entry.text),
            from: from.to_string(),
            navigable: entry.navigable,
        });
    }
    out
}

/// How much of an anchor's text is kept.
const LONGEST_TEXT: usize = 160;

/// Collapses the whitespace in a label and cuts it short.
///
/// Anchor text arrives with the page's indentation in it, and a picker row
/// showing forty spaces then a word is worse than showing nothing.
fn tidy(text: &str) -> String {
    let mut out = String::new();
    for word in text.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        if out.len() + word.len() > LONGEST_TEXT {
            out.push('…');
            break;
        }
        out.push_str(word);
    }
    out
}

// ---------------------------------------------------------------------------
// robots.txt
// ---------------------------------------------------------------------------

/// What a site's `robots.txt` says to us.
#[derive(Debug, Clone, Default)]
pub struct Robots {
    /// `(allow, pattern)`, already narrowed to the group that applies.
    rules: Vec<(bool, String)>,
    /// What the group asked to be waited between requests.
    pub delay: Duration,
}

impl Robots {
    /// A `robots.txt` that permits everything, which is what a site with
    /// none is saying.
    pub fn permissive() -> Self {
        Self::default()
    }

    /// Whether a path may be fetched.
    ///
    /// Longest matching pattern wins, and a tie goes to `Allow` — the rule
    /// every crawler settled on, and the one that reads a site's intent
    /// right when it writes `Disallow: /` then `Allow: /public/`.
    pub fn allows(&self, path: &str) -> bool {
        let mut best: Option<(usize, bool)> = None;
        for (allow, pattern) in &self.rules {
            if !matches(pattern, path) {
                continue;
            }
            let length = pattern.len();
            match best {
                Some((longest, _)) if longest > length => {}
                // A tie goes to `Allow`: an allow replaces a disallow of
                // the same length, and a disallow never replaces an allow.
                Some((longest, _)) if longest == length => {
                    if *allow {
                        best = Some((length, true));
                    }
                }
                _ => best = Some((length, *allow)),
            }
        }
        best.is_none_or(|(_, allow)| allow)
    }
}

/// Reads a site's `robots.txt`, treating anything unreadable as permission.
///
/// A missing file, a 500, a connection that failed: all of them mean the
/// site has not asked for anything, which is not the same as having asked
/// for silence. Erring the other way would mean a single bad response from
/// a CDN silently disabling the feature.
async fn robots_for(config: &HttpConfig, origin: &Url) -> Robots {
    let Ok(url) = origin.join("/robots.txt") else {
        return Robots::permissive();
    };
    match idm_net::range::read_page(config, url.as_str(), MOST_ROBOTS_BYTES).await {
        Ok(page) if page.status == 200 => parse_robots(&String::from_utf8_lossy(&page.bytes)),
        _ => Robots::permissive(),
    }
}

/// Picks out the group that applies to us and reads its rules.
///
/// A group named after this program wins over `*` when there is one, which
/// is the point of having a name at all: a site that wants to say something
/// to this program specifically can.
pub fn parse_robots(text: &str) -> Robots {
    /// One `User-agent` block, as written.
    struct Group {
        names: Vec<String>,
        rules: Vec<(bool, String)>,
        delay: Option<f64>,
    }

    let mut groups: Vec<Group> = Vec::new();
    // A run of `User-agent` lines names one group; the first rule line
    // closes the run, so the next `User-agent` starts a new group.
    let mut naming = false;

    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let Some((field, value)) = line.split_once(':') else {
            continue;
        };
        let field = field.trim().to_ascii_lowercase();
        let value = value.trim();

        match field.as_str() {
            "user-agent" => {
                if !naming || groups.is_empty() {
                    groups.push(Group { names: Vec::new(), rules: Vec::new(), delay: None });
                    naming = true;
                }
                if let Some(group) = groups.last_mut() {
                    group.names.push(value.to_ascii_lowercase());
                }
            }
            "disallow" | "allow" => {
                naming = false;
                let Some(group) = groups.last_mut() else {
                    // Rules before any `User-agent` belong to nobody.
                    continue;
                };
                // An empty `Disallow:` is the documented way to say
                // "nothing is disallowed", so it is not a rule at all.
                if field == "disallow" && value.is_empty() {
                    continue;
                }
                group.rules.push((field == "allow", value.to_owned()));
            }
            "crawl-delay" => {
                naming = false;
                if let Some(group) = groups.last_mut() {
                    group.delay = value.parse::<f64>().ok().filter(|seconds| *seconds > 0.0);
                }
            }
            _ => {}
        }
    }

    let named = |want: &str| {
        groups
            .iter()
            .find(|group| group.names.iter().any(|name| name == want))
            .map(|group| (group.rules.clone(), group.delay))
    };

    let Some((rules, delay)) = named(ROBOT_NAME).or_else(|| named("*")) else {
        return Robots::permissive();
    };
    Robots { rules, delay: delay.map(Duration::from_secs_f64).unwrap_or_default() }
}

/// Whether a `robots.txt` pattern matches a path.
///
/// A pattern is a prefix, with two extensions every crawler understands:
/// `*` stands for any run of characters, and a trailing `$` anchors the
/// match to the end of the path.
fn matches(pattern: &str, path: &str) -> bool {
    let (pattern, anchored) = match pattern.strip_suffix('$') {
        Some(rest) => (rest, true),
        None => (pattern, false),
    };

    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or("");
    let Some(mut rest) = path.strip_prefix(first) else {
        return false;
    };

    let parts: Vec<&str> = parts.collect();
    let (last, middle) = match parts.split_last() {
        Some((last, middle)) => (Some(*last), middle),
        None => (None, &[] as &[&str]),
    };

    for part in middle {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }

    match last {
        // The final literal after the last `*`. Found from the right when
        // the match is anchored, because `/*.pdf$` against `/a.pdf.pdf`
        // has to find the second `.pdf`, not the first.
        Some(part) if anchored => rest.rfind(part).is_some_and(|at| at + part.len() == rest.len()),
        Some(part) => rest.find(part).is_some(),
        // The pattern ended in `*`, so whatever is left is matched -- and
        // a `$` after a `*` constrains nothing.
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(url: &str) -> Url {
        Url::parse(url).unwrap()
    }

    #[test]
    fn a_relative_link_resolves_against_the_page_it_was_on() {
        let found = links_in(
            r#"<a href="two.bin">two</a> <a href="/three.bin">three</a>"#,
            &page("https://host.example/files/one.html"),
        );
        let urls: Vec<&str> = found.found.iter().map(|link| link.url.as_str()).collect();
        assert_eq!(
            urls,
            vec!["https://host.example/files/two.bin", "https://host.example/three.bin"]
        );
    }

    #[test]
    fn a_base_tag_moves_where_links_resolve_from() {
        // And it applies to links written before it in the document, which
        // is why resolution happens after the parse rather than during it.
        let found = links_in(
            r#"<a href="a.bin">a</a><base href="https://cdn.example/d/"><a href="b.bin">b</a>"#,
            &page("https://host.example/index.html"),
        );
        let urls: Vec<&str> = found.found.iter().map(|link| link.url.as_str()).collect();
        assert_eq!(urls, vec!["https://cdn.example/d/a.bin", "https://cdn.example/d/b.bin"]);
    }

    #[test]
    fn a_protocol_relative_link_takes_the_pages_scheme() {
        let found =
            links_in(r#"<a href="//cdn.example/x.bin">x</a>"#, &page("https://host.example/"));
        assert_eq!(found.found[0].url, "https://cdn.example/x.bin");
    }

    #[test]
    fn addresses_that_are_not_downloads_are_left_out() {
        let found = links_in(
            r##"<a href="mailto:a@b.c">mail</a>
               <a href="javascript:void(0)">script</a>
               <a href="data:text/plain,hi">data</a>
               <a href="#section">anchor</a>
               <a href="">empty</a>
               <a name="named">no href at all</a>
               <a href="real.bin">real</a>"##,
            &page("https://host.example/"),
        );
        let urls: Vec<&str> = found.found.iter().map(|link| link.url.as_str()).collect();
        assert_eq!(urls, vec!["https://host.example/real.bin"]);
    }

    #[test]
    fn the_same_address_twice_is_listed_once() {
        let found = links_in(
            r##"<a href="x.bin">one</a><a href="x.bin">two</a><a href="x.bin#part">three</a>"##,
            &page("https://host.example/"),
        );
        assert_eq!(found.found.len(), 1, "a fragment does not make it a different file");
        assert_eq!(found.found[0].text, "one", "the first label is the one kept");
    }

    #[test]
    fn an_anchors_text_comes_back_with_it() {
        let found = links_in(
            "<a href=\"x.bin\">\n   the   big\n   file\n</a>",
            &page("https://host.example/"),
        );
        assert_eq!(
            found.found[0].text, "the big file",
            "collapsed, because a page's indentation is in it"
        );
    }

    #[test]
    fn an_image_inside_an_anchor_does_not_steal_its_text() {
        // The bug this guards against: entries are pushed in document
        // order, so "the last one" when the text arrives is the image.
        let found = links_in(
            r#"<a href="big.zip"><img src="icon.png" alt="an icon"> download it</a>"#,
            &page("https://host.example/"),
        );
        let zip = found.found.iter().find(|link| link.url.ends_with("big.zip")).unwrap();
        let icon = found.found.iter().find(|link| link.url.ends_with("icon.png")).unwrap();
        assert_eq!(zip.text, "download it");
        assert_eq!(icon.text, "an icon");
        assert!(zip.navigable);
        assert!(!icon.navigable, "a src is not something to follow");
    }

    #[test]
    fn text_after_an_anchor_without_an_href_is_not_filed_under_the_last_one() {
        let found = links_in(
            r#"<a href="x.bin">right</a><a name="n">wrong</a>"#,
            &page("https://host.example/"),
        );
        assert_eq!(found.found.len(), 1);
        assert_eq!(found.found[0].text, "right");
    }

    #[test]
    fn media_sources_are_collected_too() {
        let found = links_in(
            r#"<video src="v.mp4"></video><source src="v.webm"><audio src="a.mp3"></audio>"#,
            &page("https://host.example/"),
        );
        let urls: Vec<&str> = found.found.iter().map(|link| link.url.as_str()).collect();
        assert_eq!(
            urls,
            vec![
                "https://host.example/v.mp4",
                "https://host.example/v.webm",
                "https://host.example/a.mp3"
            ]
        );
        assert!(found.found.iter().all(|link| !link.navigable));
    }

    #[test]
    fn a_page_with_more_links_than_the_cap_gives_the_cap() {
        let mut html = String::new();
        for n in 0..(MOST_LINKS + 50) {
            html.push_str(&format!("<a href=\"f{n}.bin\">f</a>"));
        }
        let found = links_in(&html, &page("https://host.example/"));
        assert_eq!(found.found.len(), MOST_LINKS);
        assert!(found.truncated, "and it says it stopped");
    }

    #[test]
    fn broken_html_still_gives_up_its_links() {
        // Not strict, because this is somebody else's markup.
        let found = links_in(
            "<p><a href=x.bin>x<div><a href='y.bin'>y</table>",
            &page("https://host.example/"),
        );
        assert_eq!(found.found.len(), 2, "{found:?}");
    }

    #[test]
    fn an_origin_is_scheme_host_and_port() {
        let a = page("https://host.example/one");
        assert!(same_origin(&a, &page("https://host.example/two")));
        assert!(
            same_origin(&a, &page("https://host.example:443/two")),
            "the default port is the port"
        );
        assert!(!same_origin(&a, &page("http://host.example/two")), "a different scheme");
        assert!(!same_origin(&a, &page("https://other.example/two")), "a different host");
        assert!(!same_origin(&a, &page("https://host.example:8443/two")), "a different port");
        assert!(
            !same_origin(&a, &page("https://sub.host.example/two")),
            "a subdomain is a different origin, whatever the registrar thinks"
        );
    }

    #[test]
    fn a_file_is_not_followed_as_a_page() {
        assert!(looks_like_a_page(&page("https://host.example/list")));
        assert!(looks_like_a_page(&page("https://host.example/list/")));
        assert!(looks_like_a_page(&page("https://host.example/index.html")));
        assert!(looks_like_a_page(&page("https://host.example/go.php?p=2")));
        assert!(!looks_like_a_page(&page("https://host.example/film.mp4")));
        assert!(!looks_like_a_page(&page("https://host.example/book.pdf")));
    }

    #[test]
    fn only_html_is_parsed() {
        assert!(is_html("text/html"));
        assert!(is_html("Text/HTML; charset=utf-8"));
        assert!(is_html("application/xhtml+xml"));
        assert!(!is_html("application/octet-stream"));
        assert!(!is_html("video/mp4"));
        assert!(!is_html(""));
    }

    // --- robots.txt -----------------------------------------------------

    #[test]
    fn a_site_with_no_rules_for_us_allows_everything() {
        let robots = parse_robots("User-agent: googlebot\nDisallow: /\n");
        assert!(robots.allows("/anything"));
    }

    #[test]
    fn a_wildcard_group_applies_to_us() {
        let robots = parse_robots("User-agent: *\nDisallow: /private/\n");
        assert!(!robots.allows("/private/x"));
        assert!(robots.allows("/public/x"));
    }

    #[test]
    fn a_group_naming_us_wins_over_the_wildcard_one() {
        let robots = parse_robots(
            "User-agent: *\nDisallow: /\n\nUser-agent: divinepulse\nDisallow: /admin/\n",
        );
        assert!(robots.allows("/files/x"), "our own group, not the catch-all");
        assert!(!robots.allows("/admin/x"));
    }

    #[test]
    fn the_longest_matching_rule_wins() {
        let robots = parse_robots("User-agent: *\nDisallow: /\nAllow: /public/\n");
        assert!(!robots.allows("/private"));
        assert!(robots.allows("/public/x"), "the more specific allow");
    }

    #[test]
    fn a_tie_goes_to_allow() {
        let robots = parse_robots("User-agent: *\nDisallow: /x\nAllow: /x\n");
        assert!(robots.allows("/x"));
    }

    #[test]
    fn an_empty_disallow_forbids_nothing() {
        // The documented way of saying "everything is fine", and read as a
        // rule matching every path it would forbid the whole site.
        let robots = parse_robots("User-agent: *\nDisallow:\n");
        assert!(robots.allows("/"));
        assert!(robots.allows("/anything"));
    }

    #[test]
    fn several_user_agent_lines_name_one_group() {
        let robots = parse_robots("User-agent: a\nUser-agent: *\nDisallow: /no/\n");
        assert!(!robots.allows("/no/x"));
    }

    #[test]
    fn comments_and_odd_spacing_are_tolerated() {
        let robots = parse_robots("# hello\n  User-Agent :  *  \n  DISALLOW : /no/  # why\n");
        assert!(!robots.allows("/no/x"));
        assert!(robots.allows("/yes/x"));
    }

    #[test]
    fn a_star_in_a_pattern_stands_for_anything() {
        let robots = parse_robots("User-agent: *\nDisallow: /a/*/secret\n");
        assert!(!robots.allows("/a/one/secret"));
        assert!(!robots.allows("/a/one/two/secret/x"));
        assert!(robots.allows("/b/one/secret"));
    }

    #[test]
    fn a_trailing_dollar_anchors_the_match() {
        let robots = parse_robots("User-agent: *\nDisallow: /*.pdf$\n");
        assert!(!robots.allows("/docs/a.pdf"));
        assert!(!robots.allows("/docs/a.pdf.pdf"), "the last one, not the first");
        assert!(robots.allows("/docs/a.pdf.html"));
    }

    #[test]
    fn a_crawl_delay_is_read() {
        let robots = parse_robots("User-agent: *\nCrawl-delay: 2.5\nDisallow: /no/\n");
        assert_eq!(robots.delay, Duration::from_secs_f64(2.5));
        assert!(!robots.allows("/no/x"), "the rules after it still count");
    }

    #[test]
    fn a_nonsense_crawl_delay_is_no_delay() {
        assert_eq!(parse_robots("User-agent: *\nCrawl-delay: soon\n").delay, Duration::ZERO);
        assert_eq!(parse_robots("User-agent: *\nCrawl-delay: -4\n").delay, Duration::ZERO);
    }

    #[test]
    fn rules_before_any_user_agent_belong_to_nobody() {
        let robots = parse_robots("Disallow: /\nUser-agent: *\nDisallow: /no/\n");
        assert!(robots.allows("/yes"), "the stray rule is not ours");
        assert!(!robots.allows("/no/x"));
    }

    #[test]
    fn an_empty_file_allows_everything() {
        assert!(parse_robots("").allows("/"));
        assert!(Robots::permissive().allows("/anything/at/all"));
    }
}
