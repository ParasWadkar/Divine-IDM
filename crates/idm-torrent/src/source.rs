//! Deciding whether an address is a torrent, and what to call it meanwhile.
//!
//! This is the one piece of torrent handling the rest of the app has to get
//! right before any torrent engine exists, because it is what routes a job:
//! an address that is a torrent must not be sent down the HTTP path, where it
//! would be fetched as a file and saved as thirty kilobytes of bencode.
//!
//! Kept free of the filesystem and the network so it can be tested as what it
//! is -- a decision about a string. A `.torrent` that turns out not to be
//! readable is reported when it is read, with the path in the message, which
//! is a better error than anything a guess here could produce.

use std::path::PathBuf;

/// Where a torrent is coming from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A `magnet:` link. Carries an info hash and no metadata: the file list,
    /// the sizes and the real name all arrive later, from the swarm.
    Magnet(String),
    /// An `http(s)` address of a `.torrent` file, which librqbit fetches
    /// itself. Not a download of that file -- see `looks_like_metadata`.
    Metadata(String),
    /// A `.torrent` on this machine, double-clicked or dropped on the window.
    File(PathBuf),
}

impl Source {
    /// Reads an address as a torrent, or decides it is not one.
    ///
    /// `None` means "this is not a torrent", which is the answer for every
    /// ordinary download and so has to be the cheap, obvious path.
    pub fn parse(address: &str) -> Option<Self> {
        let address = address.trim().trim_matches('"');
        if address.is_empty() {
            return None;
        }

        if starts_with_scheme(address, "magnet:") {
            // Only if it actually names something to fetch. `magnet:?dn=x` is
            // a well-formed URI and a useless torrent, and letting it through
            // produces a job that can never start.
            return info_hash(address).is_some().then(|| Self::Magnet(address.to_owned()));
        }

        if starts_with_scheme(address, "http://") || starts_with_scheme(address, "https://") {
            return looks_like_metadata(address).then(|| Self::Metadata(address.to_owned()));
        }

        // Anything else is only a torrent if it is named like one. No check
        // that the file exists: that belongs where it is opened, which can
        // say which path failed and why.
        has_torrent_extension(address).then(|| Self::File(PathBuf::from(address)))
    }

    /// Something to show in the list before the real name is known.
    ///
    /// A magnet's `dn=` is a hint from whoever wrote the link, not a fact
    /// about the torrent, so it is only ever a placeholder: the name the
    /// files are actually written under comes from the metadata. Falling back
    /// to the info hash is deliberate -- it is unlovely, but it is true, and
    /// it is also what the user can paste somewhere to look the torrent up.
    pub fn provisional_name(&self) -> String {
        match self {
            Self::Magnet(address) => display_name(address)
                .or_else(|| info_hash(address).map(|hash| format!("torrent {hash}")))
                .unwrap_or_else(|| "torrent".to_owned()),
            Self::Metadata(address) => leaf(address.split(['?', '#']).next().unwrap_or(address))
                .unwrap_or_else(|| "torrent".to_owned()),
            Self::File(path) => path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_else(|| "torrent".to_owned()),
        }
    }

    /// The address as librqbit wants it, for the two URL-shaped sources.
    pub fn as_url(&self) -> Option<&str> {
        match self {
            Self::Magnet(address) | Self::Metadata(address) => Some(address),
            Self::File(_) => None,
        }
    }
}

/// A scheme match that does not care about case, since `MAGNET:` is legal and
/// does turn up in links copied out of mail.
fn starts_with_scheme(address: &str, scheme: &str) -> bool {
    address.len() >= scheme.len() && address[..scheme.len()].eq_ignore_ascii_case(scheme)
}

fn has_torrent_extension(address: &str) -> bool {
    std::path::Path::new(address)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("torrent"))
}

/// Whether an http address is a `.torrent` rather than an ordinary download.
///
/// The judgement, stated plainly: a link whose path ends in `.torrent` is
/// taken to mean the torrent, not the metadata file. Downloading the file
/// itself is almost never what anyone wants -- it is a few kilobytes of
/// bencode that is only useful handed to a torrent client, which is what this
/// app now is. Someone who really wants the file can still fetch it with the
/// command line.
///
/// Only the path is examined. A query string full of parameters that happen to
/// mention `.torrent` does not count.
fn looks_like_metadata(address: &str) -> bool {
    let path = address.split(['?', '#']).next().unwrap_or(address);
    has_torrent_extension(path)
}

/// The last path segment of an address, with `.torrent` taken off.
fn leaf(path: &str) -> Option<String> {
    let name = path.trim_end_matches('/').rsplit('/').next()?;
    let name = name.strip_suffix(".torrent").unwrap_or(name);
    let decoded = percent_decode(name);
    (!decoded.is_empty()).then_some(decoded)
}

/// The `btih` info hash of a magnet link, if it has one.
///
/// Both the 40-character hex form and the 32-character base32 one are in use.
/// Neither is validated beyond its length and alphabet: this only decides
/// whether the link names a torrent at all, and librqbit does the real
/// parsing.
fn info_hash(address: &str) -> Option<String> {
    for (key, value) in query_pairs(address) {
        if !key.eq_ignore_ascii_case("xt") {
            continue;
        }
        let Some(hash) = value.strip_prefix("urn:btih:").or_else(|| value.strip_prefix("urn:btmh:"))
        else {
            continue;
        };
        let plausible = match hash.len() {
            40 => hash.bytes().all(|b| b.is_ascii_hexdigit()),
            32 => hash.bytes().all(|b| b.is_ascii_alphanumeric()),
            // A v2 `btmh` multihash is longer and hex; length alone will not
            // settle it, so the alphabet has to.
            _ => hash.len() > 40 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
        };
        if plausible {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

/// The `dn` display name of a magnet link, cleaned up enough to show.
fn display_name(address: &str) -> Option<String> {
    for (key, value) in query_pairs(address) {
        if key.eq_ignore_ascii_case("dn") {
            // `+` for a space is the older convention and still what most
            // sites emit.
            let decoded = percent_decode(&value.replace('+', " "));
            let trimmed = decoded.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }
    }
    None
}

/// The query parameters of a URI, as borrowed key/value pairs.
///
/// Written out rather than taken from a URL parser because a magnet link is
/// not a hierarchical URL -- it has no authority and no path -- and the only
/// part of it that matters here is the query.
fn query_pairs(address: &str) -> impl Iterator<Item = (&str, &str)> {
    address
        .split_once('?')
        .map(|(_, query)| query)
        .unwrap_or("")
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        // Trimmed, because a link that has been through a mail client or a
        // chat window arrives with whitespace in it -- and an info hash with
        // a trailing space is not 40 characters long, so without this the
        // link is not recognised as a torrent at all and goes down the HTTP
        // path instead.
        .map(|(key, value)| (key.trim(), value.trim()))
}

/// Percent-decoding, for display only.
///
/// Invalid escapes are left as they are rather than refused: this decodes a
/// name to show someone, and a stray `%` in a torrent title should not cost
/// the whole label.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Some(high) = (bytes[index + 1] as char).to_digit(16)
            && let Some(low) = (bytes[index + 2] as char).to_digit(16)
        {
            out.push((high * 16 + low) as u8);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "c9e15763f722f23e98a29decdfae341b98d53056";

    fn magnet(tail: &str) -> String {
        format!("magnet:?xt=urn:btih:{HASH}{tail}")
    }

    #[test]
    fn a_magnet_link_is_a_torrent() {
        assert_eq!(Source::parse(&magnet("")), Some(Source::Magnet(magnet(""))));
    }

    #[test]
    fn the_scheme_may_be_shouted() {
        // Links copied out of mail and chat clients arrive like this.
        let shouted = format!("MAGNET:?xt=urn:btih:{HASH}");
        assert!(matches!(Source::parse(&shouted), Some(Source::Magnet(_))));
    }

    #[test]
    fn a_magnet_naming_nothing_to_fetch_is_not_a_torrent() {
        // Well-formed and useless. Accepting it would put a job in the list
        // that can never start and cannot say why.
        assert_eq!(Source::parse("magnet:?dn=something"), None);
        assert_eq!(Source::parse("magnet:?xt=urn:btih:nothex"), None);
        assert_eq!(Source::parse("magnet:"), None);
    }

    #[test]
    fn whitespace_inside_a_link_does_not_hide_the_torrent() {
        // A link that has been through a mail client or a chat window comes
        // back with spaces and newlines in it. Untrimmed, the info hash is
        // not 40 characters, the link is not recognised, and it goes down
        // the HTTP path to be saved as a file called `download.bin`.
        let wrapped = format!("magnet:?xt=urn:btih:{HASH} &dn=Thing &tr=udp://x");
        assert!(matches!(Source::parse(&wrapped), Some(Source::Magnet(_))));
        assert_eq!(Source::parse(&wrapped).unwrap().provisional_name(), "Thing");

        let newlined = format!("magnet:?xt=urn:btih:{HASH}
&dn=Thing");
        assert!(matches!(Source::parse(&newlined), Some(Source::Magnet(_))));
    }

    #[test]
    fn a_base32_info_hash_counts_too() {
        let base32 = "magnet:?xt=urn:btih:ZGVOTDVPOIVD6XWJRHO43LRUDOMNKMCW";
        assert!(matches!(Source::parse(base32), Some(Source::Magnet(_))));
    }

    #[test]
    fn an_http_link_to_a_torrent_file_is_the_torrent_not_the_file() {
        // The whole point of the judgement: the bencode file on its own is
        // useless, so a link to one means "fetch this torrent".
        assert_eq!(
            Source::parse("https://releases.example.org/ubuntu.iso.torrent"),
            Some(Source::Metadata("https://releases.example.org/ubuntu.iso.torrent".into()))
        );
    }

    #[test]
    fn an_ordinary_download_is_left_alone() {
        // The cheap, overwhelmingly common answer. Everything here must stay
        // on the HTTP path.
        for address in [
            "https://example.org/ubuntu.iso",
            "https://example.org/",
            "http://example.org/a/b/c.zip",
            "ftp://example.org/pub/file.tar.gz",
            "sftp://example.org/home/file.bin",
            "not a url at all",
        ] {
            assert_eq!(Source::parse(address), None, "{address} should not be a torrent");
        }
    }

    #[test]
    fn a_query_string_mentioning_torrent_does_not_make_a_download_one() {
        // Only the path decides. A tracking parameter or a filename in a
        // query should not reroute an ordinary download into the swarm.
        assert_eq!(Source::parse("https://example.org/get?file=thing.torrent"), None);
        assert_eq!(Source::parse("https://example.org/dl.php?id=7&ref=x.torrent"), None);
    }

    #[test]
    fn a_fragment_after_the_extension_is_still_a_torrent() {
        assert!(matches!(
            Source::parse("https://example.org/a.torrent#frag"),
            Some(Source::Metadata(_))
        ));
        assert!(matches!(
            Source::parse("https://example.org/a.torrent?auth=k"),
            Some(Source::Metadata(_))
        ));
    }

    #[test]
    fn a_local_torrent_file_is_a_torrent() {
        assert_eq!(
            Source::parse(r"D:\downloads\ubuntu.torrent"),
            Some(Source::File(PathBuf::from(r"D:\downloads\ubuntu.torrent")))
        );
        // What the shell hands over for a double-clicked file, quotes and all.
        assert!(matches!(
            Source::parse("\"C:\\Users\\me\\Downloads\\x.TORRENT\"  "),
            Some(Source::File(_))
        ));
    }

    #[test]
    fn a_magnet_shows_its_display_name_until_the_real_one_arrives() {
        let link = magnet("&dn=Ubuntu+24.04.1+Desktop+%28amd64%29");
        assert_eq!(
            Source::parse(&link).unwrap().provisional_name(),
            "Ubuntu 24.04.1 Desktop (amd64)"
        );
    }

    #[test]
    fn a_magnet_with_no_display_name_shows_its_hash() {
        // Unlovely but true, and the one thing about the torrent that is
        // known for certain at this point.
        let name = Source::parse(&magnet("")).unwrap().provisional_name();
        assert!(name.contains(HASH), "{name}");
    }

    #[test]
    fn the_other_sources_name_themselves_after_the_file() {
        assert_eq!(
            Source::parse(r"D:\dl\Ubuntu 24.04.torrent").unwrap().provisional_name(),
            "Ubuntu 24.04"
        );
        assert_eq!(
            Source::parse("https://example.org/d/Debian%2013.torrent").unwrap().provisional_name(),
            "Debian 13"
        );
    }

    #[test]
    fn every_torrent_has_a_name_to_show() {
        // Never an empty label in the list, whatever shape the address took.
        for address in [
            magnet(""),
            magnet("&dn="),
            magnet("&dn=%20%20"),
            "https://example.org/a.torrent".to_owned(),
            "https://example.org/a.torrent/".to_owned(),
            r"D:.torrent".to_owned(),
        ] {
            let name = Source::parse(&address).unwrap().provisional_name();
            assert!(!name.trim().is_empty(), "{address} produced an empty name");
        }
    }

    #[test]
    fn a_bare_dot_torrent_is_not_an_address_for_a_torrent() {
        // `.torrent` alone is a hidden file with no extension, not a name
        // with one, so there is nothing here identifying a torrent. Left on
        // the HTTP path, where a wrong guess costs a small file rather than
        // a job that cannot start.
        assert_eq!(Source::parse("https://example.org/.torrent"), None);
        assert_eq!(Source::parse(".torrent"), None);
    }

    #[test]
    fn a_broken_escape_in_a_name_costs_only_itself() {
        assert_eq!(percent_decode("100%+of%20it"), "100%+of it");
        assert_eq!(percent_decode("%"), "%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("%4"), "%4");
    }

    #[test]
    fn only_url_shaped_sources_have_a_url() {
        assert!(Source::parse(&magnet("")).unwrap().as_url().is_some());
        assert!(Source::parse("https://x/a.torrent").unwrap().as_url().is_some());
        assert!(Source::parse(r"D:\a.torrent").unwrap().as_url().is_none());
    }
}
