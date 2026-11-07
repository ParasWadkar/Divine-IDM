//! Something that behaves like yt-dlp, for testing the code that drives it.
//!
//! The real one is a 17 MB download that has to be fetched deliberately,
//! and pointing it at a video site during a test would make the test a
//! question about somebody else's servers. What the engine actually needs
//! to get right is narrower than yt-dlp: the arguments it passes, the
//! progress lines it reads back, and the path it believes at the end. This
//! answers exactly those, in the format yt-dlp's own progress hook
//! documents, so the parsing is exercised against the shape it will meet.
//!
//! It is not a yt-dlp: it fetches nothing and understands one template.

use std::io::Write;
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value_of = |flag: &str| -> Option<String> {
        args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
    };

    // The behaviours a test asks for, through the address, since that is
    // the one thing it fully controls.
    let url = args.last().cloned().unwrap_or_default();
    if url.contains("refuse") {
        eprintln!("ERROR: Unsupported URL: {url}");
        std::process::exit(1);
    }

    let dir = PathBuf::from(value_of("-P").unwrap_or_else(|| ".".into()));
    let template = value_of("-o").unwrap_or_else(|| "%(title)s.%(ext)s".into());
    let name = template.replace("%(title)s", "a video").replace("%(ext)s", "mp4");
    let path = dir.join(name);

    let total: u64 = 4096;
    let unknown = url.contains("live");
    for done in [1024u64, 2048, 4096] {
        // The exact shape the engine asked for in --progress-template, with
        // absent fields rendered as NA the way yt-dlp renders them.
        let status = if done == total { "finished" } else { "downloading" };
        if unknown {
            println!("idmprogress {status} {done} NA NA");
        } else {
            println!("idmprogress {status} {done} {total} {total}");
        }
        let _ = std::io::stdout().flush();
    }

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, vec![b'v'; total as usize]);

    // What --print after_move:filepath writes: the final path, absolute,
    // after any renaming.
    let absolute = std::fs::canonicalize(&path).unwrap_or(path);
    println!("{}", absolute.display());
}
