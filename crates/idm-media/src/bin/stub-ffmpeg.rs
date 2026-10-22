//! A stand-in for ffmpeg, for tests.
//!
//! Real ffmpeg is a hundred megabytes and is not a dependency anyone should
//! acquire to run a test suite. What needs testing here is not ffmpeg — it
//! is the code around it: that the right executable is found, that it is
//! asked for its version before being trusted, that arguments arrive as
//! arguments rather than as shell text, and that a failure is reported
//! rather than swallowed.
//!
//! So this answers `-version` like ffmpeg does, writes whatever output file
//! it is told to, and records the exact argument vector it received so a
//! test can assert on it. It converts nothing.
//!
//! An output path containing `should-fail` makes it exit non-zero with
//! something on stderr, for testing the unhappy path. Carried in the
//! argument rather than in the environment because tests share a process:
//! an environment variable set by one leaks into every other running at the
//! same time.

use std::io::Write;

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();

    if arguments.iter().any(|argument| argument == "-version") {
        println!("ffmpeg version 0.0-stub Copyright (c) nobody");
        println!("built with nothing at all");
        return;
    }

    if arguments.last().is_some_and(|last| last.contains("should-fail")) {
        // ffmpeg writes everything to stderr, including the part that
        // matters, so the failure path has to cope with that.
        eprintln!("frame=  120 fps=0.0 q=-1.0 size=    1024kB");
        eprintln!("[out#0] Error muxing a packet");
        eprintln!("Conversion failed!");
        std::process::exit(1);
    }

    // The output is the last argument, as it is for ffmpeg itself.
    let Some(output) = arguments.last() else {
        eprintln!("no output path given");
        std::process::exit(2);
    };

    // One line per argument: a test that split on spaces could not tell an
    // argument containing a space from two arguments, which is exactly the
    // property being checked.
    let record = format!("{}\n", arguments.join("\n"));
    let _ = std::fs::write(format!("{output}.argv"), record);

    match std::fs::File::create(output) {
        Ok(mut file) => {
            let _ = file.write_all(b"stub output, not real media");
        }
        Err(error) => {
            eprintln!("could not write {output}: {error}");
            std::process::exit(3);
        }
    }
}
