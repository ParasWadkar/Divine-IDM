//! Expanding `[1-100]` in a URL into the downloads it stands for.
//!
//! A numbered series — `page-[1-50].jpg`, `disc[01-09].iso` — is otherwise
//! fifty or nine separate things to paste in by hand. The notation is the
//! one every download manager uses, so it is the one people will try.
//!
//! Zero padding is taken from how the range is written: `[01-09]` means
//! `01`, `02`, and a server asked for `1` would answer with a 404 while
//! looking as though the range were simply wrong.

/// How many a single pattern may stand for.
///
/// A cap rather than trust, because this is reachable from a browser
/// extension and `[1-99999999]` would otherwise be a way to make the engine
/// queue a hundred million jobs and run out of memory doing it. Chosen to
/// be far beyond any real series and far below anything dangerous.
pub const MAX_EXPANSION: usize = 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchError {
    /// `[9-1]`, which is either a typo or a misunderstanding.
    Backwards {
        from: u64,
        to: u64,
    },
    TooMany {
        wanted: usize,
        limit: usize,
    },
}

impl std::fmt::Display for BatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Backwards { from, to } => {
                write!(f, "the range [{from}-{to}] counts downwards; write it as [{to}-{from}]")
            }
            Self::TooMany { wanted, limit } => {
                write!(f, "that pattern stands for {wanted} downloads, and the limit is {limit}")
            }
        }
    }
}

impl std::error::Error for BatchError {}

/// Whether a URL contains a range worth expanding.
pub fn is_pattern(url: &str) -> bool {
    find_range(url).is_some()
}

/// Expands the first `[from-to]` in `url`.
///
/// One range per URL, not every range. Two ranges could mean the cross
/// product or the pairs, people disagree about which, and a guess here
/// either queues the wrong thing or queues far too much of it.
pub fn expand(url: &str) -> Result<Vec<String>, BatchError> {
    let Some(range) = find_range(url) else {
        return Ok(vec![url.to_owned()]);
    };

    if range.from > range.to {
        return Err(BatchError::Backwards { from: range.from, to: range.to });
    }
    let count = (range.to - range.from + 1) as usize;
    if count > MAX_EXPANSION {
        return Err(BatchError::TooMany { wanted: count, limit: MAX_EXPANSION });
    }

    let before = &url[..range.start];
    let after = &url[range.end..];
    Ok((range.from..=range.to)
        .map(|n| format!("{before}{:0width$}{after}", n, width = range.width))
        .collect())
}

struct Range {
    start: usize,
    end: usize,
    from: u64,
    to: u64,
    /// Digits to pad to, taken from how the range was written.
    width: usize,
}

fn find_range(url: &str) -> Option<Range> {
    let bytes = url.as_bytes();
    let start = url.find('[')?;
    let end = url[start..].find(']')? + start + 1;
    let inside = &url[start + 1..end - 1];

    let (from_text, to_text) = inside.split_once('-')?;
    if from_text.is_empty() || to_text.is_empty() {
        return None;
    }
    if !from_text.bytes().all(|b| b.is_ascii_digit())
        || !to_text.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let _ = bytes;

    Some(Range {
        start,
        end,
        from: from_text.parse().ok()?,
        to: to_text.parse().ok()?,
        // `[01-09]` pads to two; `[1-9]` does not pad at all.
        width: from_text.len().max(if from_text.starts_with('0') { to_text.len() } else { 0 }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_url_expands_to_itself() {
        assert_eq!(expand("https://x.invalid/a.bin").unwrap(), vec!["https://x.invalid/a.bin"]);
        assert!(!is_pattern("https://x.invalid/a.bin"));
    }

    #[test]
    fn a_range_becomes_one_url_each() {
        let urls = expand("https://x.invalid/page-[1-4].jpg").unwrap();
        assert_eq!(
            urls,
            vec![
                "https://x.invalid/page-1.jpg",
                "https://x.invalid/page-2.jpg",
                "https://x.invalid/page-3.jpg",
                "https://x.invalid/page-4.jpg",
            ]
        );
    }

    #[test]
    fn zero_padding_is_kept_as_written() {
        // A server asked for `1` where it wanted `01` answers 404, and the
        // range looks wrong rather than the padding.
        let urls = expand("https://x.invalid/disc[08-10].iso").unwrap();
        assert_eq!(urls[0], "https://x.invalid/disc08.iso");
        assert_eq!(urls[1], "https://x.invalid/disc09.iso");
        assert_eq!(urls[2], "https://x.invalid/disc10.iso");
    }

    #[test]
    fn an_unpadded_range_stays_unpadded() {
        let urls = expand("https://x.invalid/[8-10].bin").unwrap();
        assert_eq!(urls[0], "https://x.invalid/8.bin");
        assert_eq!(urls[2], "https://x.invalid/10.bin");
    }

    #[test]
    fn a_single_value_range_is_one_download() {
        assert_eq!(expand("https://x.invalid/[7-7].bin").unwrap(), vec!["https://x.invalid/7.bin"]);
    }

    #[test]
    fn a_backwards_range_is_refused_with_the_fix_in_the_message() {
        let error = expand("https://x.invalid/[9-1].bin").unwrap_err();
        assert!(matches!(error, BatchError::Backwards { .. }));
        assert!(error.to_string().contains("[1-9]"), "{error}");
    }

    #[test]
    fn an_enormous_range_is_refused_rather_than_queued() {
        // Reachable from a browser extension, so this is a limit rather
        // than a convention.
        let error = expand("https://x.invalid/[1-99999999].bin").unwrap_err();
        assert!(matches!(error, BatchError::TooMany { .. }));
        assert!(error.to_string().contains("1000"), "{error}");
    }

    #[test]
    fn exactly_the_limit_is_allowed() {
        assert_eq!(expand("https://x.invalid/[1-1000].bin").unwrap().len(), MAX_EXPANSION);
        assert!(expand("https://x.invalid/[1-1001].bin").is_err());
    }

    #[test]
    fn brackets_that_are_not_a_range_are_left_alone() {
        // Square brackets appear in real URLs, and a signed one full of
        // base64 must not be mangled into a hundred requests.
        for url in [
            "https://x.invalid/a[b].bin",
            "https://x.invalid/a[].bin",
            "https://x.invalid/a[1-].bin",
            "https://x.invalid/a[-9].bin",
            "https://x.invalid/a[1-b].bin",
            "https://x.invalid/[::1]/a.bin",
            "https://x.invalid/a.bin?k=[abc-def]",
        ] {
            assert!(!is_pattern(url), "{url} should not look like a range");
            assert_eq!(expand(url).unwrap(), vec![url.to_owned()], "{url}");
        }
    }

    #[test]
    fn only_the_first_range_is_expanded() {
        // Two ranges could mean the pairs or the cross product; people
        // disagree, and guessing queues either the wrong thing or far too
        // much of it.
        let urls = expand("https://x.invalid/[1-2]/[5-6].bin").unwrap();
        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0], "https://x.invalid/1/[5-6].bin");
    }

    #[test]
    fn a_range_anywhere_in_the_url_works() {
        assert_eq!(expand("https://[1-2].x.invalid/a.bin").unwrap().len(), 2);
        assert_eq!(expand("https://x.invalid/a.bin?page=[1-3]").unwrap().len(), 3);
    }
}
