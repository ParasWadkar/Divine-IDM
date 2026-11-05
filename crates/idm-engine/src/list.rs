//! Lists of downloads, written out and read back in.
//!
//! Three formats, and only one of them is ours.
//!
//! # Writing: aria2's input file
//!
//! An export is only worth anything if something else can read it, so the
//! format written here is the one `aria2c --input-file` takes: an address
//! per line, mirrors for the same file separated by tabs on that line, and
//! indented `key=value` lines for the rest. An exported list is therefore a
//! list `aria2c -i` will fetch, and a list this reads back.
//!
//! Inventing a format instead would have produced a file that only this
//! program understands, which is the opposite of what exporting is for.
//!
//! # Reading: that, and a bare list of addresses
//!
//! Most lists people actually have are an address per line and nothing
//! else — pasted out of a page, a forum post, a text file somebody mailed
//! them. That is read too, because a format that rejected it would be
//! refusing the common case to protect a convention it invented.
//!
//! # Reading: IDM's `.ef2`
//!
//! The format Internet Download Manager exports, since somebody moving from
//! it will have one. There is no published specification; the shape is
//! `<` and `>` lines around each entry, the address on its own line, then
//! `referer:` and `User-Agent:` lines, with community tools adding
//! `cookies:`, `postdata:`, `username:`, `password:`, `filename:` and
//! `filepath:`.
//!
//! **Three of those are deliberately dropped.** `cookies`, `username` and
//! `password` are credentials sitting in a text file that has been mailed
//! around; importing them would quietly start sending somebody's session to
//! a server because a file said to. And an entry with `postdata` is
//! **skipped rather than imported**: it describes a form submission, so
//! fetching the same address with a GET downloads something else — usually
//! a login page, saved under the name of the file that was wanted.
//! `filepath` is ignored because a file is not allowed to choose where
//! downloads land.

/// One download, as a list describes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Wanted {
    pub url: String,
    pub mirrors: Vec<String>,
    /// A suggested leaf name. Never a path: see [`leaf`].
    pub filename: Option<String>,
    pub referrer: Option<String>,
}

/// How many entries will be taken out of one file.
///
/// A list is a file from somewhere else, and "somewhere else" includes a
/// generated one with a million lines in it.
pub const MOST_ENTRIES: usize = 2_000;

/// How long a line may be before it is ignored.
pub const LONGEST_LINE: usize = 8_192;

/// Writes a list in aria2's input-file format.
pub fn write(items: &[Wanted]) -> String {
    let mut out = String::from("# DivinePulse download list\n");
    out.push_str("# aria2c -i thisfile  will fetch it too\n");
    for item in items {
        // Mirrors go on the address's own line, tab separated, which is how
        // aria2 says "these are the same file".
        let mut line = item.url.clone();
        for mirror in &item.mirrors {
            line.push('\t');
            line.push_str(mirror);
        }
        out.push_str(&line);
        out.push('\n');
        if let Some(name) = &item.filename {
            out.push_str("  out=");
            out.push_str(name);
            out.push('\n');
        }
        if let Some(referrer) = &item.referrer {
            out.push_str("  referer=");
            out.push_str(referrer);
            out.push('\n');
        }
    }
    out
}

/// Reads whichever of the two text formats the file turns out to be.
///
/// Dispatched on the name rather than sniffed, because the two text
/// formats are read by the same function anyway and `.ef2` is the only one
/// that needs telling apart.
pub fn read(name: &str, text: &str) -> Vec<Wanted> {
    if name.to_ascii_lowercase().ends_with(".ef2") { read_ef2(text) } else { read_text(text) }
}

/// Reads a bare list of addresses, or an aria2 input file.
pub fn read_text(text: &str) -> Vec<Wanted> {
    let mut items: Vec<Wanted> = Vec::new();

    for line in text.lines().take(MOST_ENTRIES * 4) {
        if line.len() > LONGEST_LINE {
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }

        // Indented is aria2 for "an option belonging to the line above".
        // Without one above it, there is nothing for it to belong to.
        let indented = line.starts_with(' ') || line.starts_with('\t');
        if indented && let Some(last) = items.last_mut() {
            apply_option(last, trimmed);
            continue;
        }

        let mut addresses = trimmed.split('\t').map(str::trim).filter(|part| is_address(part));
        let Some(url) = addresses.next() else {
            continue;
        };
        if items.len() >= MOST_ENTRIES {
            break;
        }
        items.push(Wanted {
            url: url.to_owned(),
            mirrors: addresses.map(str::to_owned).collect(),
            ..Wanted::default()
        });
    }

    items
}

/// One `key=value` from an aria2 input file, where it is one we honour.
fn apply_option(item: &mut Wanted, option: &str) {
    let Some((key, value)) = option.split_once('=') else {
        return;
    };
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    match key.trim() {
        // aria2's spelling of the output name, and its spelling of the
        // referrer, misspelling and all -- it follows the HTTP header.
        "out" => item.filename = leaf(value),
        "referer" => item.referrer = Some(value.to_owned()),
        // Everything else aria2 understands -- dir, header, split,
        // max-connection-per-server -- is either not ours to honour or is
        // a setting this program already has its own answer for.
        _ => {}
    }
}

/// Reads IDM's export format.
pub fn read_ef2(text: &str) -> Vec<Wanted> {
    let mut items = Vec::new();
    let mut current: Option<Wanted> = None;
    let mut posted = false;

    for line in text.lines() {
        if line.len() > LONGEST_LINE {
            continue;
        }
        let trimmed = line.trim();

        if trimmed == "<" {
            current = Some(Wanted::default());
            posted = false;
            continue;
        }
        if trimmed == ">" {
            // See the module docs: a form submission fetched as a GET is a
            // different file, usually a login page under the right name.
            if let Some(item) = current.take()
                && !item.url.is_empty()
                && !posted
                && items.len() < MOST_ENTRIES
            {
                items.push(item);
            }
            continue;
        }

        let Some(item) = current.as_mut() else {
            continue;
        };

        if item.url.is_empty() {
            // The first line inside a block is the address, with no
            // keyword in front of it.
            if is_address(trimmed) {
                item.url = trimmed.to_owned();
            }
            continue;
        }

        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match key.trim().to_ascii_lowercase().as_str() {
            "referer" | "referrer" => item.referrer = Some(value.to_owned()),
            "filename" => item.filename = leaf(value),
            "postdata" | "pd" => posted = true,
            // cookies, username, password: credentials, deliberately not
            // imported. user-agent and filepath: not ours to take from a
            // file either. See the module docs for both.
            _ => {}
        }
    }

    items
}

/// The leaf of a suggested name, or nothing.
///
/// A list is a file from elsewhere and the name in it is free text. The
/// engine sanitises names again before anything is written, so this is not
/// the only guard — but handing it a path at all would mean the folder in
/// the list was something anybody had to think about.
pub fn leaf(name: &str) -> Option<String> {
    let leaf = name.rsplit(['/', '\\']).next()?.trim();
    (!leaf.is_empty() && leaf != "." && leaf != "..").then(|| leaf.to_owned())
}

/// Whether something is an address worth trying.
///
/// http and https only. A list can say `file:///C:/Windows/...`, and a
/// program that fetched it would be copying local files on the say-so of a
/// text file. The engine refuses those anyway; refusing here means the
/// count shown to the person is the count that will actually start.
fn is_address(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    (lower.starts_with("http://") || lower.starts_with("https://")) && text.len() <= LONGEST_LINE
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(url: &str) -> Wanted {
        Wanted { url: url.to_owned(), ..Wanted::default() }
    }

    #[test]
    fn a_bare_list_of_addresses_is_read() {
        // The commonest list there is: pasted out of a page.
        let read = read_text("https://a.invalid/1.bin\nhttps://a.invalid/2.bin\n");
        assert_eq!(read, vec![one("https://a.invalid/1.bin"), one("https://a.invalid/2.bin")]);
    }

    #[test]
    fn blank_lines_and_comments_are_skipped() {
        let read = read_text("# a list\n\nhttps://a.invalid/1.bin\n; and another\n");
        assert_eq!(read.len(), 1);
    }

    #[test]
    fn anything_that_is_not_an_http_address_is_skipped() {
        // `file:///C:/Windows/...` in a list would otherwise be a text file
        // asking this program to copy local files.
        let read = read_text(
            "file:///C:/Windows/System32/config/SAM\n\
             javascript:alert(1)\n\
             ftp://a.invalid/x\n\
             just some words\n\
             https://a.invalid/ok.bin\n",
        );
        assert_eq!(read, vec![one("https://a.invalid/ok.bin")]);
    }

    #[test]
    fn mirrors_on_one_line_belong_to_one_download() {
        let read = read_text("https://a.invalid/x.bin\thttps://b.invalid/x.bin\n");
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].mirrors, vec!["https://b.invalid/x.bin"]);
    }

    #[test]
    fn the_indented_options_are_read() {
        let read = read_text(
            "https://a.invalid/x.bin\n  out=renamed.bin\n  referer=https://a.invalid/page\n",
        );
        assert_eq!(read[0].filename.as_deref(), Some("renamed.bin"));
        assert_eq!(read[0].referrer.as_deref(), Some("https://a.invalid/page"));
    }

    #[test]
    fn an_option_naming_a_folder_keeps_only_the_leaf() {
        // A list choosing where a download lands is a list choosing the
        // Startup folder.
        let read = read_text("https://a.invalid/x.bin\n  out=../../Startup/x.exe\n");
        assert_eq!(read[0].filename.as_deref(), Some("x.exe"));
        let read = read_text("https://a.invalid/x.bin\n  out=C:\\Windows\\x.dll\n");
        assert_eq!(read[0].filename.as_deref(), Some("x.dll"));
    }

    #[test]
    fn an_option_with_nothing_above_it_is_dropped() {
        assert!(read_text("  out=x.bin\n").is_empty());
    }

    #[test]
    fn a_setting_of_aria2s_that_is_not_ours_is_ignored() {
        let read = read_text("https://a.invalid/x.bin\n  dir=C:\\Elsewhere\n  split=16\n");
        assert_eq!(read[0].filename, None);
    }

    #[test]
    fn what_is_written_is_read_back_the_same() {
        let items = vec![
            Wanted {
                url: "https://a.invalid/x.bin".into(),
                mirrors: vec!["https://b.invalid/x.bin".into()],
                filename: Some("x.bin".into()),
                referrer: Some("https://a.invalid/page".into()),
            },
            one("https://a.invalid/plain.bin"),
        ];
        assert_eq!(read_text(&write(&items)), items);
    }

    #[test]
    fn an_ef2_entry_is_read() {
        let read = read_ef2(
            "<\n\
             https://img.invalid/IMG_0001.jpg\n\
             referer: https://img.invalid/album/\n\
             User-Agent: Mozilla/5.0\n\
             >\n",
        );
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].url, "https://img.invalid/IMG_0001.jpg");
        assert_eq!(read[0].referrer.as_deref(), Some("https://img.invalid/album/"));
    }

    #[test]
    fn several_ef2_entries_are_read() {
        let text = "<\nhttps://a.invalid/1.bin\n>\n<\nhttps://a.invalid/2.bin\n>\n";
        assert_eq!(read_ef2(text).len(), 2);
    }

    #[test]
    fn ef2_credentials_are_not_imported() {
        // The point of the whole format decision. A file that has been
        // mailed around should not start sending somebody's session to a
        // server because it said to.
        let read = read_ef2(
            "<\n\
             https://a.invalid/x.bin\n\
             cookies: session=secret\n\
             username: someone\n\
             password: hunter2\n\
             >\n",
        );
        assert_eq!(read, vec![one("https://a.invalid/x.bin")]);
    }

    #[test]
    fn an_ef2_entry_that_needs_a_post_is_skipped() {
        // A form submission fetched as a GET is a different file: usually
        // a login page, saved under the name of the one that was wanted.
        let read = read_ef2("<\nhttps://a.invalid/x.bin\npostdata: id=4&go=1\n>\n");
        assert!(read.is_empty(), "{read:?}");
    }

    #[test]
    fn an_ef2_filename_keeps_only_its_leaf() {
        let read = read_ef2("<\nhttps://a.invalid/x.bin\nfilename: ..\\..\\x.exe\n>\n");
        assert_eq!(read[0].filename.as_deref(), Some("x.exe"));
    }

    #[test]
    fn an_unfinished_ef2_block_is_not_imported() {
        // No closing `>`, so nothing said the entry was complete.
        assert!(read_ef2("<\nhttps://a.invalid/x.bin\n").is_empty());
    }

    #[test]
    fn the_format_is_chosen_by_the_file_name() {
        let ef2 = "<\nhttps://a.invalid/x.bin\n>\n";
        assert_eq!(read("saved.ef2", ef2).len(), 1);
        assert_eq!(read("SAVED.EF2", ef2).len(), 1);
        // The same text read as a plain list: the address is still an
        // address, and the brackets are not.
        assert_eq!(read("saved.txt", ef2).len(), 1);
    }

    #[test]
    fn a_list_longer_than_the_cap_stops_at_it() {
        let many: String =
            (0..MOST_ENTRIES + 50).map(|n| format!("https://a.invalid/{n}.bin\n")).collect();
        assert_eq!(read_text(&many).len(), MOST_ENTRIES);
    }

    #[test]
    fn an_implausibly_long_line_is_ignored() {
        let long = format!("https://a.invalid/{}\n", "x".repeat(LONGEST_LINE));
        assert!(read_text(&long).is_empty());
    }
}
