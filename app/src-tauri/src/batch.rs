//! One address standing for many: `part[1-24].rar`.
//!
//! A set of files that differ only by a number is the usual shape of a
//! large download split into parts, and pasting twenty-four addresses by
//! hand to fetch one of them is the kind of work a download manager exists
//! to not make anyone do.
//!
//! Only the window's own address box is expanded. A browser hands its
//! downloads over through the endpoint instead, and a URL it chose may
//! legitimately contain brackets -- expanding one of those would turn a
//! single download the person asked for into a hundred they did not.

/// Beyond this, the pattern was almost certainly a mistake.
///
/// Refused rather than trimmed: quietly fetching the first thousand of
/// something is harder to notice, and harder to undo, than being told the
/// range is too wide.
const MOST: usize = 1000;

/// Every address `input` stands for, in order.
///
/// A plain address stands only for itself, so the common case returns a
/// list of one and nothing about it changes.
pub fn expand(input: &str) -> Result<Vec<String>, String> {
    // A torrent address is never a range. A magnet's `dn=` is a title
    // somebody else wrote, and titles contain things like `Series[1-12]`,
    // which would become twelve jobs pointed at info hashes nobody has.
    //
    // Asked of `Source::parse` rather than restated, so this and the engine's
    // own expander cannot come to disagree about what a torrent is. That
    // there are two expanders at all is a wart: this one runs for the window,
    // because the window shows how many jobs were queued and the engine's
    // reply does not carry a count.
    if idm_torrent::Source::parse(input).is_some() {
        return Ok(vec![input.to_owned()]);
    }

    let Some((head, body, tail)) = split(input) else {
        return Ok(vec![input.to_owned()]);
    };
    let Some((first, last)) = body.split_once('-') else {
        return Ok(vec![input.to_owned()]);
    };

    // Anything that is not a plain number is not a range. `[a-z]` and a
    // stray bracket in a path both land here and are left alone, which is
    // the safe way to be wrong about it.
    if first.is_empty()
        || last.is_empty()
        || !first.bytes().all(|b| b.is_ascii_digit())
        || !last.bytes().all(|b| b.is_ascii_digit())
    {
        return Ok(vec![input.to_owned()]);
    }

    let (Ok(from), Ok(to)) = (first.parse::<u64>(), last.parse::<u64>()) else {
        return Ok(vec![input.to_owned()]);
    };
    if from > to {
        return Ok(vec![input.to_owned()]);
    }

    let count = (to - from + 1) as usize;
    if count > MOST {
        return Err(format!("{count} addresses is too many at once; {MOST} is the most"));
    }

    // `[01-12]` means every number is written to the same width, because
    // that is how the files were named.
    let width = if first.starts_with('0') && first.len() > 1 { first.len() } else { 0 };

    Ok((from..=to).map(|n| format!("{head}{:0>width$}{tail}", n, width = width)).collect())
}

/// The first bracketed run, and what sits either side of it.
///
/// The first and not the widest: one range is what these names have, and
/// picking between several would be guessing at which one was meant.
fn split(input: &str) -> Option<(&str, &str, &str)> {
    let open = input.find('[')?;
    let close = input[open..].find(']')? + open;
    Some((&input[..open], &input[open + 1..close], &input[close + 1..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_torrent_address_is_never_a_range() {
        // The window's own path into the engine, so the guard has to be here
        // too: without it a magnet whose title contains `[1-12]` is twelve
        // jobs before the engine ever sees it.
        const LINK: &str = concat!(
            "magnet:?xt=urn:btih:c9e15763f722f23e98a29decdfae341b98d53056",
            "&dn=Some.Series[1-12].1080p"
        );
        assert_eq!(expand(LINK).unwrap(), vec![LINK.to_owned()]);
        let path = r"D:\dl\pack[1-9].torrent";
        assert_eq!(expand(path).unwrap(), vec![path.to_owned()]);
    }

    #[test]
    fn an_ordinary_address_stands_only_for_itself() {
        let one = expand("https://example.invalid/file.zip").unwrap();
        assert_eq!(one, ["https://example.invalid/file.zip"]);
    }

    #[test]
    fn a_range_becomes_one_address_for_each_number() {
        let many = expand("https://example.invalid/part[1-3].rar").unwrap();
        assert_eq!(
            many,
            [
                "https://example.invalid/part1.rar",
                "https://example.invalid/part2.rar",
                "https://example.invalid/part3.rar",
            ]
        );
    }

    #[test]
    fn a_padded_range_keeps_the_width_the_files_were_named_with() {
        // part1.rar and part01.rar are different files, and the padding is
        // the only thing that says which was meant.
        let many = expand("https://example.invalid/part[08-11].rar").unwrap();
        assert_eq!(
            many,
            [
                "https://example.invalid/part08.rar",
                "https://example.invalid/part09.rar",
                "https://example.invalid/part10.rar",
                "https://example.invalid/part11.rar",
            ]
        );
    }

    #[test]
    fn a_single_number_range_is_still_a_range() {
        assert_eq!(expand("https://example.invalid/p[7-7].bin").unwrap().len(), 1);
        assert_eq!(expand("https://example.invalid/p[7-7].bin").unwrap()[0], "https://example.invalid/p7.bin");
    }

    #[test]
    fn brackets_that_are_not_a_range_are_left_alone() {
        // A URL is allowed to contain these, and a download the person
        // asked for once must not quietly become many.
        for address in [
            "https://example.invalid/a[b-c].zip",
            "https://example.invalid/a[10].zip",
            "https://example.invalid/a[-5].zip",
            "https://example.invalid/a[5-].zip",
            "https://example.invalid/weird[.zip",
            "https://example.invalid/set[9-2].zip",
        ] {
            assert_eq!(expand(address).unwrap(), [address], "{address}");
        }
    }

    #[test]
    fn only_the_first_range_is_expanded() {
        let many = expand("https://example.invalid/[1-2]/x[5-9].bin").unwrap();
        assert_eq!(many.len(), 2);
        assert_eq!(many[0], "https://example.invalid/1/x[5-9].bin");
    }

    #[test]
    fn too_wide_a_range_is_refused_rather_than_trimmed() {
        let error = expand("https://example.invalid/f[1-5000].bin").unwrap_err();
        assert!(error.contains("5000"), "{error}");
        assert!(error.contains("1000"), "the limit should be said: {error}");
    }

    #[test]
    fn the_widest_allowed_range_is_allowed() {
        assert_eq!(expand("https://example.invalid/f[1-1000].bin").unwrap().len(), MOST);
    }
}
