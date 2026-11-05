//! Asking Windows Defender what it makes of a finished download.
//!
//! # Why MpCmdRun and not AMSI
//!
//! AMSI is the interface a program is meant to use to have content scanned,
//! and it is the wrong one here. It wants the *content*: a buffer, or a
//! handle it reads from, which for a four-gigabyte ISO means streaming four
//! gigabytes through this process to be told something the file system
//! already knows. `MpCmdRun.exe -Scan -File` hands over a path and lets the
//! scanner read it directly.
//!
//! It is also the same scan the user would get from the right-click menu,
//! which matters for what the answer means: a verdict here is one they can
//! reproduce without this program, against definitions they already trust.
//!
//! # Nothing is ever removed
//!
//! `-DisableRemediation` is passed on every scan and is not optional. Without
//! it, Defender's default action on a detection is to quarantine or delete,
//! and the first a user would know of it is a finished download that is not
//! there. A download manager that silently destroys what it fetched is worse
//! than one that does not scan at all, however right the scanner was.
//!
//! So a detection fails the job, leaves the file exactly where it is, and
//! says what was found. Deleting it is then a thing the person does, having
//! been told what they are deleting and why.
//!
//! # A verdict is not always available
//!
//! Defender may be absent, replaced by another product, switched off by
//! policy, or simply slow. All of those are [`Verdict::Unknown`], which does
//! not fail the download: refusing to deliver a file because the scanner
//! could not be reached would be a feature that breaks downloading whenever
//! it breaks.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// What the scanner said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Clean,
    /// Something was found. The name is Defender's own, because it is what
    /// a person will search for.
    Infected {
        name: String,
    },
    /// No answer, and the reason. Never fails a download.
    Unknown {
        why: String,
    },
}

/// How long a scan may take before it is abandoned.
///
/// Generous, because a first scan of a large archive genuinely takes a
/// while and the file is already downloaded by this point. Bounded, because
/// a scanner that has hung must not hold a finished download forever.
pub const LONGEST_SCAN: Duration = Duration::from_secs(180);

/// How large a file will be sent for scanning.
///
/// Past this the scan costs more than it is worth: reading forty gigabytes
/// twice to check a disc image is a long stall for a verdict that is
/// almost always Clean, and real-time protection sees the writes anyway.
/// Said as Unknown rather than Clean, because "not scanned" and "scanned
/// and fine" are different facts.
pub const LARGEST_SCAN_BYTES: u64 = 8 << 30;

/// Where `MpCmdRun.exe` lives, if it does.
///
/// Three places, in order, and the order matters. Defender updates itself
/// by unpacking a new platform version beside the old ones and switching to
/// it, so the copy under `Platform` is the running engine and the one in
/// Program Files is whatever shipped with Windows, possibly years old.
///
/// `Platform\CurrentVersion` is a junction Defender maintains to the
/// version it is actually running, which is a far better answer than any
/// amount of guessing at directory names — so it is tried first, and the
/// version comparison below exists only for a machine that does not have
/// it.
#[cfg(windows)]
pub fn tool() -> Option<PathBuf> {
    let platform = std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .map(|dir| dir.join(r"Microsoft\Windows Defender\Platform"));

    if let Some(platform) = platform {
        let current = platform.join(r"CurrentVersion\MpCmdRun.exe");
        if current.is_file() {
            return Some(current);
        }

        if let Ok(entries) = std::fs::read_dir(&platform) {
            let mut versions: Vec<(Vec<u64>, PathBuf)> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .filter_map(|path| {
                    let name = path.file_name()?.to_str()?;
                    Some((version_of(name), path))
                })
                .collect();
            versions.sort();
            if let Some((_, newest)) = versions.into_iter().next_back() {
                let exe = newest.join("MpCmdRun.exe");
                if exe.is_file() {
                    return Some(exe);
                }
            }
        }
    }

    std::env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .map(|dir| dir.join(r"Windows Defender\MpCmdRun.exe"))
        .filter(|path| path.is_file())
}

/// A platform directory name as numbers, for comparing.
///
/// Numbers rather than text, because text gets this wrong in exactly the
/// case it matters: `4.18.26100.4-0` is newer than `4.18.26090.9-0` and
/// sorts *before* it as a string, since `1` is less than `9` three
/// characters in. A name that is not a version at all compares as nothing,
/// which puts it below every real one.
fn version_of(name: &str) -> Vec<u64> {
    name.split(['.', '-']).map_while(|part| part.parse::<u64>().ok()).collect()
}

#[cfg(not(windows))]
pub fn tool() -> Option<PathBuf> {
    None
}

/// Reads a verdict out of what `MpCmdRun.exe` returned.
///
/// Pure, and the only part of this worth testing properly: the exit codes
/// and the wording are the whole protocol, and they can be exercised from
/// strings without a scanner, a threat, or a file.
///
/// `0` is clean. `2` is *not* simply "infected", which is the trap here:
/// Microsoft document it as covering malware found and not remediated,
/// malware needing the user to finish remediating, **and scanning errors**.
/// Reading it as a detection on its own would fail downloads because a
/// scanner could not open a file. So a `2` is a detection only when the
/// report also shows one, and otherwise it is no answer at all.
///
/// Anything else is likewise [`Verdict::Unknown`] rather than a guess in
/// either direction -- guessing Clean hides a detection, and guessing
/// Infected fails a download over a scanner that would not start.
pub fn verdict(code: Option<i32>, output: &str) -> Verdict {
    match code {
        Some(0) => Verdict::Clean,
        Some(2) => match detection(output) {
            Some(name) => Verdict::Infected { name },
            // `-DisableRemediation` is passed, so a real detection always
            // ends "found and not remediated" with the threat named. A 2
            // with nothing named is the scanning-error case.
            None => Verdict::Unknown {
                why: "the scanner reported a problem but named no threat".into(),
            },
        },
        Some(code) => Verdict::Unknown { why: format!("the scanner exited with {code}") },
        None => Verdict::Unknown { why: "the scanner was stopped before it answered".into() },
    }
}

/// The threat's name, when the report shows a detection at all.
///
/// Two pieces of evidence, either of which is enough. The report names what
/// it found on a `Threat  : Trojan:Win32/Wacatac.B!ml` line, and summarises
/// above it with `... found 1 threats.`. Neither layout is documented as an
/// interface, so the summary line is the fallback when the name cannot be
/// read -- a detection nobody can name is still a detection, and a count of
/// zero is not one.
fn detection(output: &str) -> Option<String> {
    for line in output.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Threat")
            && let Some(name) = rest.trim_start().strip_prefix(':')
            && !name.trim().is_empty()
        {
            return Some(name.trim().to_owned());
        }
    }

    // `found 3 threats.` -- and `found 0 threats.`, which is not one.
    for line in output.lines() {
        let lower = line.to_ascii_lowercase();
        let Some(at) = lower.find("found ") else {
            continue;
        };
        let rest = &lower[at + "found ".len()..];
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if !digits.is_empty()
            && rest[digits.len()..].trim_start().starts_with("threat")
            && digits.parse::<u64>().is_ok_and(|count| count > 0)
        {
            return Some("an unnamed detection".to_owned());
        }
    }
    None
}

/// Scans one file and says what Defender made of it.
pub async fn scan(tool: &Path, file: &Path) -> Verdict {
    match tokio::fs::metadata(file).await {
        Ok(data) if data.len() > LARGEST_SCAN_BYTES => {
            return Verdict::Unknown { why: "too large to be worth scanning".into() };
        }
        Ok(_) => {}
        Err(error) => return Verdict::Unknown { why: error.to_string() },
    }

    let mut command = tokio::process::Command::new(tool);
    command.args([
        "-Scan",
        // 3 is "scan the path given", as against a quick or full scan of
        // the machine, which is emphatically not what is wanted here.
        "-ScanType",
        "3",
        "-File",
    ]);
    command.arg(file);
    // Never optional. See the module docs: without it a detection means a
    // finished download quietly disappearing.
    command.arg("-DisableRemediation");
    command.stdin(std::process::Stdio::null());

    #[cfg(windows)]
    {
        // No console window. This runs behind a GUI, and a black rectangle
        // flashing up after every download would be the most visible thing
        // about the feature.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let run = command.output();
    let output = match tokio::time::timeout(LONGEST_SCAN, run).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => return Verdict::Unknown { why: error.to_string() },
        Err(_) => {
            return Verdict::Unknown {
                why: format!("the scan took longer than {} seconds", LONGEST_SCAN.as_secs()),
            };
        }
    };

    let said = String::from_utf8_lossy(&output.stdout);
    verdict(output.status.code(), &said)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A report shaped like the real thing.
    fn report(threat: &str) -> String {
        format!(
            "Scan starting...\n\
             Scan finished.\n\
             Scanning thing.exe found 1 threats.\n\
             \n\
             ---------------- Threat information ---------------\n\
             Threat                  : {threat}\n\
             Resources               : 1 total\n"
        )
    }

    #[test]
    fn nothing_found_is_clean() {
        assert_eq!(verdict(Some(0), "Scan starting...\nScan finished.\n"), Verdict::Clean);
    }

    #[test]
    fn a_detection_is_reported_with_its_name() {
        assert_eq!(
            verdict(Some(2), &report("Trojan:Win32/Wacatac.B!ml")),
            Verdict::Infected { name: "Trojan:Win32/Wacatac.B!ml".into() }
        );
    }

    #[test]
    fn a_two_with_no_threat_in_the_report_is_not_a_detection() {
        // The trap this whole function exists for. Microsoft document exit
        // code 2 as covering malware found and not remediated, malware
        // needing the user to finish remediating, *and scanning errors* --
        // so reading it as a detection on its own would fail downloads
        // because a scanner could not open a file.
        match verdict(Some(2), "Scan starting...\nERROR: the file is in use\n") {
            Verdict::Unknown { why } => assert!(why.contains("named no threat"), "{why}"),
            other => panic!("a scanning error is not a detection: {other:?}"),
        }
    }

    #[test]
    fn a_count_of_none_is_not_a_detection() {
        assert!(detection("Scanning thing.exe found 0 threats.").is_none());
    }

    #[test]
    fn a_detection_without_a_readable_name_still_stands() {
        // The report layout is not a documented interface, so the name is a
        // convenience and the detection is not.
        match verdict(Some(2), "Scanning thing.exe found 1 threats.\n") {
            Verdict::Infected { name } => assert_eq!(name, "an unnamed detection"),
            other => panic!("a detection must survive an unreadable report: {other:?}"),
        }
    }

    #[test]
    fn an_exit_code_nobody_documented_is_not_a_guess() {
        match verdict(Some(1), "") {
            Verdict::Unknown { why } => assert!(why.contains('1'), "{why}"),
            other => panic!("expected no verdict, got {other:?}"),
        }
        assert!(matches!(verdict(None, ""), Verdict::Unknown { .. }));
    }

    #[test]
    fn the_threat_line_is_read_whatever_the_spacing() {
        assert_eq!(detection("Threat:X/Y\n").as_deref(), Some("X/Y"));
        assert_eq!(detection("   Threat    :    X/Y    \n").as_deref(), Some("X/Y"));
        assert_eq!(detection("Threat                  : X/Y").as_deref(), Some("X/Y"));
    }

    #[test]
    fn the_named_threat_wins_over_the_summary_line() {
        assert_eq!(
            detection("Scanning thing.exe found 1 threats.\nThreat : Real/Name\n").as_deref(),
            Some("Real/Name")
        );
    }

    #[test]
    #[cfg(windows)]
    fn a_newer_platform_version_wins_even_where_text_would_say_otherwise() {
        // The case that made this numeric: 26100 is newer than 26090 and
        // sorts before it as a string, because `1` is less than `9` three
        // characters in.
        assert!(version_of("4.18.26100.4-0") > version_of("4.18.26090.9-0"));
        assert!(version_of("4.18.24090.11-0") > version_of("4.18.23110.3-0"));
        // And the directory Defender keeps beside them is not a version, so
        // it never wins the comparison by accident -- it is picked first,
        // deliberately, or not at all.
        assert!(version_of("CurrentVersion") < version_of("4.18.26090.9-0"));
    }

    #[test]
    fn a_clean_report_names_nothing() {
        assert!(detection("Scan starting...\nScan finished.\n").is_none());
        assert!(detection("").is_none());
        // Verbatim from a real run. "found no threats" has the word and no
        // number, which is exactly the shape a sloppier match would read as
        // a detection.
        assert!(detection("Scanning ordinary.bin found no threats.\n").is_none());
    }
}
