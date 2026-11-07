//! A post-download hook that does nothing but record how it was called.
//!
//! `hook`'s own tests cover what may be run and how the placeholders are
//! filled, which is the security boundary and deserves the attention it
//! gets. What none of them cover is whether the thing is ever actually
//! launched, because none of them launch anything: `hook::run` is called
//! from exactly one place in the product and from nowhere in the tests. A
//! hook that silently never fires looks the same as one that fired and did
//! nothing.
//!
//! Where to write is taken from `--log=`, an argument, rather than from
//! the environment. Two tests in one binary share an environment and run
//! at once, so an env var would have them writing to each other's files --
//! which is how the first version of this failed.
//!
//! Every argument is written, one per line, including `--log=` itself: the
//! record is "how it was called", not an edited version of it. One line
//! each so a path with a space in it -- the case a shell would ruin and
//! this program must not -- comes back whole or visibly does not.

use std::io::Write;

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();

    let Some(log) =
        arguments.iter().find_map(|argument| argument.strip_prefix("--log=").map(str::to_owned))
    else {
        eprintln!("stub-hook needs --log=<path>");
        std::process::exit(2);
    };

    let mut text = String::new();
    for argument in &arguments {
        text.push_str(argument);
        text.push('\n');
    }

    // Appended, so a second run shows up as a second run rather than
    // overwriting the first: a hook that fires twice per download is a
    // fault worth catching.
    let mut file =
        std::fs::OpenOptions::new().create(true).append(true).open(&log).expect("open the log");
    file.write_all(text.as_bytes()).expect("write the log");

    if arguments.iter().any(|argument| argument == "--fail") {
        eprintln!("stub-hook was told to fail");
        std::process::exit(3);
    }
}
