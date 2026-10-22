//! Running ffmpeg and yt-dlp, carefully.
//!
//! These are the only external programs this project runs, and everything
//! they are handed — URLs, titles, filenames — came from a web page. The
//! rules here exist because of that:
//!
//! * **An argv array, never a shell string.** A filename containing `&` or a
//!   quote is syntax to a shell and data to `CreateProcess`. There is no
//!   escaping function here to get wrong, because nothing is escaped.
//! * **An absolute path, resolved once.** Running `ffmpeg` by bare name lets
//!   the current directory and `PATH` decide what executes, and the current
//!   directory is wherever the user's download landed.
//! * **A checksum when one is pinned**, verified on the file about to run
//!   rather than at install time, since the two are not the same file if
//!   anything replaced it in between.
//! * **Nothing is downloaded automatically.** A missing sidecar is reported
//!   so the user can decide; fetching a hundred megabytes from the internet
//!   because a download needed muxing is not a decision to take on someone's
//!   behalf.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{MediaError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Ffmpeg,
    Ffprobe,
    YtDlp,
}

impl Tool {
    pub fn executable(self) -> &'static str {
        match self {
            Self::Ffmpeg => "ffmpeg",
            Self::Ffprobe => "ffprobe",
            Self::YtDlp => "yt-dlp",
        }
    }

    /// What to say when it is missing, in terms of what the user wanted.
    pub fn purpose(self) -> &'static str {
        match self {
            Self::Ffmpeg => "combining audio and video, and converting containers",
            Self::Ffprobe => "inspecting what a stream contains",
            Self::YtDlp => "finding the media behind a page",
        }
    }
}

/// A located, checked executable.
#[derive(Debug, Clone)]
pub struct Sidecar {
    pub tool: Tool,
    pub path: PathBuf,
    pub version: String,
}

/// Where sidecars are looked for, in order.
///
/// The bundled directory first, so a pinned, known build wins over whatever
/// happens to be on `PATH` — the opposite order would let an unrelated
/// ffmpeg somewhere in the user's environment silently take over.
pub fn search_paths(bundled: Option<&Path>) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(dir) = bundled {
        paths.push(dir.to_path_buf());
    }
    if let Some(exe) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf))
    {
        paths.push(exe.join("sidecars"));
        paths.push(exe);
    }
    paths
}

/// Finds `tool`, preferring a bundled copy, falling back to `PATH`.
pub fn find(tool: Tool, bundled: Option<&Path>) -> Option<PathBuf> {
    let name = format!("{}{}", tool.executable(), std::env::consts::EXE_SUFFIX);

    for directory in search_paths(bundled) {
        let candidate = directory.join(&name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    // `PATH` last, and resolved to an absolute path here so that what was
    // checked and what runs are the same file.
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|directory| {
        let candidate = directory.join(&name);
        candidate.is_file().then_some(candidate)
    })
}

/// Finds `tool` and confirms it runs, returning what it says it is.
///
/// The version call is the smoke test: a file of the right name that cannot
/// execute — a stub, a truncated download, something built for another
/// architecture — fails here, in one place, rather than halfway through a
/// mux with a diagnostic about the video.
pub fn probe(tool: Tool, bundled: Option<&Path>) -> Result<Sidecar> {
    let Some(path) = find(tool, bundled) else {
        return Err(MediaError::Sidecar(format!(
            "{} was not found, and it is needed for {}",
            tool.executable(),
            tool.purpose()
        )));
    };

    let output = Command::new(&path)
        .arg(match tool {
            Tool::YtDlp => "--version",
            _ => "-version",
        })
        .output()
        .map_err(|error| {
            MediaError::Sidecar(format!("{} could not be run: {error}", path.display()))
        })?;

    if !output.status.success() {
        return Err(MediaError::Sidecar(format!(
            "{} exited with {} when asked for its version",
            path.display(),
            output.status
        )));
    }

    let version = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned();

    Ok(Sidecar { tool, path, version })
}

/// Checks a file against a pinned SHA-256.
///
/// Checked on the file that is about to run, not once at install time: the
/// point is to notice if it was replaced in between, and a check that
/// happened earlier cannot.
pub fn verify_checksum(path: &Path, expected_hex: &str) -> Result<()> {
    let bytes = std::fs::read(path)?;
    let actual = sha256_hex(&bytes);
    if !actual.eq_ignore_ascii_case(expected_hex.trim()) {
        return Err(MediaError::Sidecar(format!(
            "{} does not match its pinned checksum (expected {expected_hex}, found {actual})",
            path.display()
        )));
    }
    Ok(())
}

impl Sidecar {
    /// Builds a command with the arguments given, and nothing else.
    ///
    /// Every argument is passed as its own element, so a filename that looks
    /// like an option or like shell syntax is still just a filename.
    pub fn command<I, S>(&self, arguments: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let mut command = Command::new(&self.path);
        command.args(arguments);
        command
    }

    /// Runs it, returning stderr when it fails.
    ///
    /// ffmpeg writes everything to stderr including ordinary progress, so
    /// only the tail is kept: the useful part of a failure is the end.
    pub fn run<I, S>(&self, arguments: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let output = self.command(arguments).output().map_err(|error| {
            MediaError::Sidecar(format!("could not run {}: {error}", self.tool.executable()))
        })?;

        if output.status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(6).collect();
        Err(MediaError::Sidecar(format!(
            "{} failed: {}",
            self.tool.executable(),
            tail.into_iter().rev().collect::<Vec<_>>().join(" / ")
        )))
    }
}

/// Arguments that join a video file and an audio file without re-encoding.
///
/// `-c copy` is the whole point: the segments were already encoded by
/// whoever published them, and re-encoding would cost minutes of processor
/// time to produce something visibly worse.
pub fn mux_arguments(video: &Path, audio: Option<&Path>, output: &Path) -> Vec<std::ffi::OsString> {
    let mut arguments: Vec<std::ffi::OsString> =
        vec!["-hide_banner".into(), "-loglevel".into(), "error".into()];
    // Overwrite: the caller has already decided on the name, and a prompt
    // from a subprocess nobody can see would hang the download for ever.
    arguments.push("-y".into());
    arguments.push("-i".into());
    arguments.push(video.into());
    if let Some(audio) = audio {
        arguments.push("-i".into());
        arguments.push(audio.into());
        arguments.push("-map".into());
        arguments.push("0:v:0".into());
        arguments.push("-map".into());
        arguments.push("1:a:0".into());
    }
    arguments.push("-c".into());
    arguments.push("copy".into());
    // MPEG-TS timestamps start wherever the stream was joined; without this
    // a remuxed MP4 can begin with a long stretch of nothing.
    arguments.push("-movflags".into());
    arguments.push("+faststart".into());
    arguments.push(output.into());
    arguments
}

/// SHA-256, implemented here to keep this crate free of a dependency used in
/// exactly one place.
fn sha256_hex(bytes: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let mut message = bytes.to_vec();
    let length_bits = (bytes.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&length_bits.to_be_bytes());

    for chunk in message.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (index, word) in chunk.as_chunks::<4>().0.iter().enumerate() {
            w[index] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for index in 16..64 {
            let s0 = w[index - 15].rotate_right(7)
                ^ w[index - 15].rotate_right(18)
                ^ (w[index - 15] >> 3);
            let s1 = w[index - 2].rotate_right(17)
                ^ w[index - 2].rotate_right(19)
                ^ (w[index - 2] >> 10);
            w[index] = w[index - 16].wrapping_add(s0).wrapping_add(w[index - 7]).wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choose = (e & f) ^ (!e & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(choose)
                .wrapping_add(K[index])
                .wrapping_add(w[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(majority);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        for (slot, value) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(value);
        }
    }

    h.iter().map(|word| format!("{word:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_answers() {
        // Without these the checksum pinning is decoration.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // Longer than one block, so the padding path is exercised too.
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn a_file_that_does_not_match_its_pin_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ffmpeg.exe");
        std::fs::write(&path, b"abc").unwrap();

        verify_checksum(&path, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
            .expect("the real checksum should pass");

        let error = verify_checksum(&path, "0".repeat(64).as_str()).unwrap_err();
        assert!(error.to_string().contains("pinned checksum"), "{error}");
    }

    #[test]
    fn a_bundled_copy_is_preferred_over_whatever_is_on_the_path() {
        // The other order lets an unrelated ffmpeg somewhere in the user's
        // environment silently take over from the build that was verified.
        let dir = tempfile::tempdir().unwrap();
        let bundled = dir.path().join("sidecars");
        std::fs::create_dir_all(&bundled).unwrap();
        let name = format!("ffmpeg{}", std::env::consts::EXE_SUFFIX);
        std::fs::write(bundled.join(&name), b"not really ffmpeg").unwrap();

        let found = find(Tool::Ffmpeg, Some(&bundled)).expect("the bundled copy");
        assert_eq!(found, bundled.join(&name));
    }

    #[test]
    fn a_missing_sidecar_says_what_it_was_wanted_for() {
        let dir = tempfile::tempdir().unwrap();
        // A directory with nothing in it, and a tool name nothing on PATH
        // will answer to.
        let error = probe(Tool::Ffprobe, Some(dir.path()));
        if let Err(error) = error {
            let text = error.to_string();
            assert!(text.contains("ffprobe"), "{text}");
            assert!(text.contains("inspecting"), "the message should say why: {text}");
        }
        // If ffprobe really is installed, finding it is also a correct
        // outcome and there is nothing to assert about the message.
    }

    #[test]
    fn mux_arguments_copy_rather_than_re_encode() {
        // Re-encoding costs minutes of processor time to produce something
        // visibly worse than what was already downloaded.
        let arguments = mux_arguments(
            Path::new("video.ts"),
            Some(Path::new("audio.m4a")),
            Path::new("out.mp4"),
        );
        let joined: Vec<String> =
            arguments.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert!(joined.windows(2).any(|pair| pair == ["-c", "copy"]), "{joined:?}");
        assert!(joined.contains(&"-y".to_owned()), "a hidden prompt would hang for ever");
        assert_eq!(joined.last().unwrap(), "out.mp4");
    }

    #[test]
    fn a_single_track_needs_no_stream_mapping() {
        let arguments = mux_arguments(Path::new("v.ts"), None, Path::new("out.mp4"));
        let joined: Vec<String> =
            arguments.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert!(!joined.contains(&"-map".to_owned()), "{joined:?}");
    }

    #[test]
    fn arguments_are_passed_as_themselves_not_as_shell_text() {
        // A filename is data. This is the property that makes a title taken
        // from a web page safe to pass along.
        let sidecar =
            Sidecar { tool: Tool::Ffmpeg, path: PathBuf::from("ffmpeg"), version: String::new() };
        let hostile = r#"a" & del /q C:\ & echo ".mp4"#;
        let command = sidecar.command(["-i", hostile]);
        let passed: Vec<String> =
            command.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(passed, vec!["-i".to_owned(), hostile.to_owned()]);
    }
}
