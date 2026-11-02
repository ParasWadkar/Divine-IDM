//! Finding the figure a publisher said a download should hash to.
//!
//! Checking a finished file is only worth something if the figure it is
//! checked against came from somewhere other than the bytes themselves.
//! Usually that figure is already published a few metres from the link:
//! beside the file as `SHA256SUMS`, or on the page the download started
//! from. This goes and reads it, so nobody has to go hunting.
//!
//! # What it refuses to do
//!
//! Guess. A hash that merely *appears* near a download says nothing on its
//! own, and checking against the wrong one reports a mismatch that looks
//! exactly like a corrupted file — which is worse than reporting nothing at
//! all, because it sends somebody chasing a fault that was never there. So a
//! sums file counts only when one of its lines names this file, and a page
//! counts only when a hash sits next to a mention of this filename or is the
//! single hash on the page.
//!
//! # The algorithm is deliberately not decided here
//!
//! A hash's length already says which kind it is, and that is worked out
//! where the comparison happens. This module's job ends at "here is a hex
//! string, and here is the address it was read from" — the address being the
//! half that lets somebody judge the figure as well as the file.

use reqwest::{Client, Url};

/// Hash lengths worth recognising: MD5, SHA-1, SHA-256.
///
/// Longest first, and that is a preference rather than just an order. A page
/// that publishes both a SHA-256 and an MD5 should be taken at its strongest.
const HASH_LENGTHS: [usize; 3] = [64, 40, 32];

/// Most that will be read from any one address.
///
/// A directory-wide sums file for a large mirror runs to tens of kilobytes
/// and a download page to a few hundred. Anything past this is not a checksum
/// listing, and spending the bandwidth to confirm that is not worth it.
const MAX_BODY: usize = 2 << 20;

/// How far from a mention of the filename a hash may sit and still be taken
/// as belonging to it.
///
/// Sized for a table row or a list item — the filename in one cell and the
/// hash in the next. Wide enough to survive the markup between them once
/// tags are stripped, narrow enough that the next file's row does not reach.
const NEAR: usize = 512;

/// A published hash, and where it was read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub hash: String,
    /// The address it came from, shown to the user alongside the verdict:
    /// a hash is only as good as its provenance, and this is the provenance.
    pub source: String,
}

/// Whether a token is shaped like one of the hashes this recognises.
fn is_hash(token: &str) -> bool {
    HASH_LENGTHS.contains(&token.len()) && token.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The last path segment of an address, which is what sibling checksum files
/// are named after.
///
/// Not the same thing as the download's filename, which may have come from a
/// `Content-Disposition` header and have no relation to the URL at all. Both
/// are tried when matching, but only this one can build a sibling address.
fn url_name(url: &Url) -> Option<String> {
    let last = url.path_segments()?.next_back()?;
    if last.is_empty() { None } else { Some(last.to_owned()) }
}

/// Addresses worth asking for `url`'s hash, in the order worth asking.
///
/// Kept short on purpose. Each entry is a request that will usually 404, and
/// the list is walked until something answers, so its length is a direct cost
/// on the common case. These are the conventions that actually appear in the
/// wild; a longer list would buy very little and be felt every time.
pub fn candidates(url: &Url) -> Vec<Url> {
    let mut out = Vec::new();
    let mut push = |relative: &str| {
        if let Ok(joined) = url.join(relative) {
            out.push(joined);
        }
    };

    if let Some(name) = url_name(url) {
        push(&format!("{name}.sha256"));
        push(&format!("{name}.sha256sum"));
    }
    push("SHA256SUMS");
    push("sha256sum.txt");
    push("CHECKSUMS");
    if let Some(name) = url_name(url) {
        push(&format!("{name}.md5"));
    }
    push("MD5SUMS");
    out
}

/// `SHA256 (name) = hash`, which the BSD tools and some mirrors emit.
fn bsd_line(line: &str) -> Option<(&str, &str)> {
    let open = line.find('(')?;
    let close = line.rfind(')')?;
    if close < open {
        return None;
    }
    let name = &line[open + 1..close];
    let hash = line[close + 1..].trim_start().strip_prefix('=')?.split_whitespace().next()?;
    is_hash(hash).then_some((hash, name))
}

/// Whether a name from a sums file refers to the file being checked.
///
/// Compared on the last path component, because a sums file at the top of a
/// tree lists its entries by relative path while the download knows only its
/// own name.
fn names_match(name: &str, wanted: &[&str]) -> bool {
    let base = name.trim().trim_start_matches('*').rsplit(['/', '\\']).next().unwrap_or(name);
    wanted.iter().any(|want| base.eq_ignore_ascii_case(want))
}

/// A hash for one of `wanted` in `sha256sum`- or BSD-style text.
///
/// A file holding nothing but a hash is also accepted. That shape carries no
/// filename to match on, but it only ever arrives from an address derived
/// from this download's own — `<file>.sha256` — so the address is the match.
pub fn find_in_sums(text: &str, wanted: &[&str]) -> Option<String> {
    let mut lone: Option<&str> = None;
    let mut lone_seen = 0usize;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some((hash, name)) = bsd_line(line) {
            if names_match(name, wanted) {
                return Some(hash.to_ascii_lowercase());
            }
            continue;
        }

        let (first, rest) = match line.find(char::is_whitespace) {
            Some(at) => (&line[..at], line[at..].trim_start()),
            None => (line, ""),
        };
        if !is_hash(first) {
            continue;
        }
        if rest.is_empty() {
            lone_seen += 1;
            lone = lone.or(Some(first));
            continue;
        }
        if names_match(rest, wanted) {
            return Some(first.to_ascii_lowercase());
        }
    }

    // Only when it is unambiguous: a file with two bare hashes in it says
    // nothing about which belongs to what.
    (lone_seen == 1).then(|| lone.unwrap().to_ascii_lowercase())
}

/// Markup replaced by spaces, so text either side of a tag does not run
/// together and offsets stay roughly proportional to what a reader sees.
fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut inside = false;
    for ch in html.chars() {
        match ch {
            '<' => {
                inside = true;
                out.push(' ');
            }
            '>' if inside => {
                inside = false;
                out.push(' ');
            }
            _ if inside => {}
            _ => out.push(ch),
        }
    }
    out
}

/// Every hash-shaped run in `text`, with where it starts.
///
/// A run counts only if the whole of it is one hash's worth of hex and it is
/// not butted against other alphanumerics. Without that second rule a long
/// identifier that happens to contain sixty-four hex characters would read as
/// a SHA-256.
fn hash_tokens(text: &str) -> Vec<(usize, &str)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_hexdigit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
            i += 1;
        }
        let bounded = (start == 0 || !bytes[start - 1].is_ascii_alphanumeric())
            && (i == bytes.len() || !bytes[i].is_ascii_alphanumeric());
        if bounded && HASH_LENGTHS.contains(&(i - start)) {
            out.push((start, &text[start..i]));
        }
    }
    out
}

/// A hash for one of `wanted` somewhere in a page.
pub fn find_in_page(text: &str, wanted: &[&str]) -> Option<String> {
    let plain = strip_tags(text);
    let tokens = hash_tokens(&plain);
    if tokens.is_empty() {
        return None;
    }

    let haystack = plain.to_ascii_lowercase();
    let mentions: Vec<usize> = wanted
        .iter()
        .flat_map(|want| {
            let needle = want.to_ascii_lowercase();
            haystack.match_indices(&needle).map(|(at, _)| at).collect::<Vec<_>>()
        })
        .collect();

    // Strongest hash near the filename, nearest one breaking a tie. Pages
    // routinely print MD5, SHA-1 and SHA-256 together on one row, so
    // proximity alone would pick whichever the author happened to list first.
    let near = tokens
        .iter()
        .filter_map(|(at, token)| {
            let distance = mentions.iter().map(|m| at.abs_diff(*m)).min()?;
            (distance <= NEAR).then_some((token.len(), std::cmp::Reverse(distance), *token))
        })
        .max();
    if let Some((_, _, token)) = near {
        return Some(token.to_ascii_lowercase());
    }

    // Nothing to tie a hash to the name, but a page carrying exactly one
    // hash is not ambiguous about what it is for.
    let mut distinct: Vec<String> = tokens.iter().map(|(_, t)| t.to_ascii_lowercase()).collect();
    distinct.sort_unstable();
    distinct.dedup();
    (distinct.len() == 1).then(|| distinct.pop().unwrap())
}

/// Reads at most [`MAX_BODY`] of a response body as text.
///
/// Streamed rather than taken whole: an address that was expected to hold a
/// few lines of hex is under no obligation to, and `text()` would hand over
/// however much it was sent.
async fn bounded_text(mut response: reqwest::Response) -> String {
    let mut body = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        let room = MAX_BODY.saturating_sub(body.len());
        if room == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..chunk.len().min(room)]);
    }
    // Lossy, because a sums file with a stray byte in it still has usable
    // lines, and every byte this looks for is ASCII anyway.
    String::from_utf8_lossy(&body).into_owned()
}

/// Whether a response looks like markup rather than a checksum listing.
///
/// Worth knowing, because a mirror answering a missing `SHA256SUMS` with a
/// styled 404 page returns `200 OK`, and a hash printed somewhere in that
/// page's furniture would otherwise be read as this file's.
fn is_html(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("text/html"))
}

/// Looks for a published hash for a download, nearby first.
///
/// Returns `None` whenever nothing can be tied to this file with confidence,
/// which is the common outcome and not an error: most downloads have no
/// published hash, and the caller falls back to asking.
pub async fn discover(
    client: &Client,
    url: &str,
    referrer: Option<&str>,
    filename: &str,
) -> Option<Found> {
    let parsed = Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }

    let from_url = url_name(&parsed);
    let mut wanted: Vec<&str> = vec![filename];
    if let Some(name) = from_url.as_deref()
        && !name.eq_ignore_ascii_case(filename)
    {
        wanted.push(name);
    }

    for candidate in candidates(&parsed) {
        let Ok(response) = client.get(candidate.clone()).send().await else { continue };
        if !response.status().is_success() || is_html(&response) {
            continue;
        }
        let text = bounded_text(response).await;
        if let Some(hash) = find_in_sums(&text, &wanted) {
            return Some(Found { hash, source: candidate.to_string() });
        }
    }

    // The page the link was followed from, last: it is the least precise of
    // the places to look, and the only one that needs a filename to match on.
    let page = Url::parse(referrer?).ok()?;
    if !matches!(page.scheme(), "http" | "https") {
        return None;
    }
    let response = client.get(page.clone()).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let text = bounded_text(response).await;
    // Tried as a listing first: plenty of "pages" are plain-text indexes,
    // and a line naming the file beats proximity in markup.
    let hash = find_in_sums(&text, &wanted).or_else(|| find_in_page(&text, &wanted))?;
    Some(Found { hash, source: page.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ISO_SHA256: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f901a2b3c4d5e6f708192a3b4c5d6e7f801";
    const OTHER_SHA256: &str = "ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100";
    const ISO_MD5: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn the_coreutils_format_is_matched_by_filename() {
        let text = format!(
            "{OTHER_SHA256}  other.iso\n{ISO_SHA256}  debian.iso\n{OTHER_SHA256}  third.iso\n"
        );
        assert_eq!(find_in_sums(&text, &["debian.iso"]), Some(ISO_SHA256.to_owned()));
    }

    #[test]
    fn binary_mode_and_relative_paths_still_match() {
        // `sha256sum -b` marks the name, and a sums file at the top of a tree
        // lists entries by path while the download knows only its own name.
        let text = format!("{ISO_SHA256} *./images/debian.iso\n");
        assert_eq!(find_in_sums(&text, &["debian.iso"]), Some(ISO_SHA256.to_owned()));
    }

    #[test]
    fn the_bsd_format_is_understood_too() {
        let text = format!("SHA256 (debian.iso) = {ISO_SHA256}\n");
        assert_eq!(find_in_sums(&text, &["debian.iso"]), Some(ISO_SHA256.to_owned()));
    }

    #[test]
    fn a_file_holding_only_a_hash_is_taken_as_this_files_hash() {
        // What `<file>.sha256` usually contains. The address did the matching.
        assert_eq!(
            find_in_sums(&format!("{ISO_SHA256}\n"), &["debian.iso"]),
            Some(ISO_SHA256.to_owned())
        );
    }

    #[test]
    fn two_bare_hashes_are_refused_rather_than_picked_between() {
        let text = format!("{ISO_SHA256}\n{OTHER_SHA256}\n");
        assert_eq!(find_in_sums(&text, &["debian.iso"]), None);
    }

    #[test]
    fn a_sums_file_not_mentioning_this_file_yields_nothing() {
        let text = format!("{OTHER_SHA256}  something-else.iso\n");
        assert_eq!(find_in_sums(&text, &["debian.iso"]), None);
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let text = format!("# Generated by somebody\n\n{ISO_SHA256}  debian.iso\n");
        assert_eq!(find_in_sums(&text, &["debian.iso"]), Some(ISO_SHA256.to_owned()));
    }

    #[test]
    fn a_page_hash_beside_the_filename_is_found_through_the_markup() {
        let html = format!(
            "<table><tr><td>debian.iso</td><td><code>{ISO_SHA256}</code></td></tr></table>"
        );
        assert_eq!(find_in_page(&html, &["debian.iso"]), Some(ISO_SHA256.to_owned()));
    }

    #[test]
    fn the_strongest_hash_on_the_row_is_preferred() {
        // Pages list MD5, SHA-1 and SHA-256 together constantly, and the
        // weakest is as often as not printed first.
        let html = format!("<p>debian.iso — MD5: {ISO_MD5} SHA256: {ISO_SHA256}</p>");
        assert_eq!(find_in_page(&html, &["debian.iso"]), Some(ISO_SHA256.to_owned()));
    }

    #[test]
    fn the_hash_belonging_to_another_file_is_not_taken() {
        // Two rows far enough apart that only one is near the name.
        let filler = " ".repeat(NEAR * 2);
        let html = format!(
            "<tr><td>other.iso</td><td>{OTHER_SHA256}</td></tr>{filler}\
             <tr><td>debian.iso</td><td>{ISO_SHA256}</td></tr>"
        );
        assert_eq!(find_in_page(&html, &["debian.iso"]), Some(ISO_SHA256.to_owned()));
    }

    #[test]
    fn a_page_with_one_hash_and_no_filename_is_still_unambiguous() {
        let html = format!("<p>The checksum of the download is {ISO_SHA256}.</p>");
        assert_eq!(find_in_page(&html, &["debian.iso"]), Some(ISO_SHA256.to_owned()));
    }

    #[test]
    fn a_page_of_many_hashes_and_no_filename_is_refused() {
        // Guessing here would report a mismatch that reads as corruption.
        let html = format!("<p>{ISO_SHA256}</p><p>{OTHER_SHA256}</p>");
        assert_eq!(find_in_page(&html, &["debian.iso"]), None);
    }

    #[test]
    fn hex_inside_a_longer_identifier_is_not_a_hash() {
        let html = format!("<p>debian.iso build z{ISO_SHA256}z</p>");
        assert_eq!(find_in_page(&html, &["debian.iso"]), None);
    }

    #[test]
    fn a_longer_hex_blob_is_not_mistaken_for_a_sha256() {
        // A 128-character run contains a 64-character one; only the whole run
        // counts, so this is not a SHA-512 misread as a SHA-256.
        let blob = format!("{ISO_SHA256}{OTHER_SHA256}");
        let html = format!("<p>debian.iso {blob}</p>");
        assert_eq!(find_in_page(&html, &["debian.iso"]), None);
    }

    #[test]
    fn sibling_addresses_are_built_from_the_urls_own_last_segment() {
        let url = Url::parse("https://example.test/images/debian.iso").unwrap();
        let built: Vec<String> = candidates(&url).iter().map(Url::to_string).collect();
        assert!(
            built.contains(&"https://example.test/images/debian.iso.sha256".to_owned()),
            "{built:?}"
        );
        assert!(built.contains(&"https://example.test/images/SHA256SUMS".to_owned()), "{built:?}");
        // A sibling, never a child: joining must replace the last segment.
        assert!(
            !built.iter().any(|u| u.contains("debian.iso/")),
            "a candidate was built below the file: {built:?}"
        );
    }

    #[test]
    fn a_query_string_does_not_become_part_of_a_sibling_name() {
        let url = Url::parse("https://example.test/d/debian.iso?token=abc").unwrap();
        let built: Vec<String> = candidates(&url).iter().map(Url::to_string).collect();
        assert!(
            built.contains(&"https://example.test/d/debian.iso.sha256".to_owned()),
            "{built:?}"
        );
    }

    #[test]
    fn an_upper_case_published_hash_matches_a_lower_case_one() {
        let text = format!("{}  debian.iso\n", ISO_SHA256.to_ascii_uppercase());
        assert_eq!(find_in_sums(&text, &["debian.iso"]), Some(ISO_SHA256.to_owned()));
    }
}
