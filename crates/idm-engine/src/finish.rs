//! What happens to a download after the last byte is written.
//!
//! Until now there was nothing here: the transfer ended and the job was
//! marked finished in the same breath. Everything that wants to happen in
//! between — scanning it, running a hook over it, unpacking it — would
//! otherwise be a few more lines each inside `start`, which is already the
//! longest function in the engine and is about *starting* things.
//!
//! So this is the other end, and it is deliberately a list of steps with
//! one rule: a step may refuse to deliver the download, and a step that
//! merely could not run does not. The difference is the whole design. A
//! scanner that found a trojan has said something; a scanner that would not
//! start has not, and failing the download on its silence would mean a
//! feature that breaks downloading whenever it breaks.

use std::path::Path;

use idm_ipc::Settings;

use crate::failure::{Failure, Kind};
use crate::scan::{self, Verdict};

/// What the steps turned up, for the record rather than for a decision.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// `None` when scanning was switched off or no scanner was found.
    pub scan: Option<Verdict>,
}

/// Turns a verdict into a refusal, or into nothing.
///
/// Pure, and the one piece of judgement in the module: only a detection
/// stops a download, and the message has to be useful to somebody looking
/// at a file they now have and did not expect to be told about.
pub fn judge(verdict: &Verdict) -> Option<Failure> {
    match verdict {
        Verdict::Clean | Verdict::Unknown { .. } => None,
        Verdict::Infected { name } => Some(Failure {
            kind: Kind::ContentInfected,
            summary: format!("Windows Defender found {name}"),
            // Says where the file is, because it is still there and that is
            // the surprising part. Nothing was removed, and somebody who
            // reads only the detail should still learn that.
            detail: format!(
                "Windows Defender reported {name} in this file. \
                 Nothing has been deleted or quarantined: the file is still \
                 where it was downloaded, and removing it is yours to decide."
            ),
            // Never retried. Downloading it again would find the same thing.
            transient: false,
        }),
    }
}

/// Runs every after-the-bytes step over a finished download.
///
/// Returns the refusal when one of them refuses, and the report either way,
/// because what a step found is worth recording even when it changed
/// nothing.
pub async fn run(settings: &Settings, file: &Path) -> (Report, Option<Failure>) {
    let mut report = Report::default();

    if settings.scan_downloads
        && let Some(tool) = scan::tool()
    {
        let verdict = scan::scan(&tool, file).await;
        let refusal = judge(&verdict);
        report.scan = Some(verdict);
        if refusal.is_some() {
            // Returned before any later step, which is the point of
            // scanning first: nothing else should touch a file that has
            // just been called a trojan.
            return (report, refusal);
        }
    }

    (report, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_file_is_delivered() {
        assert!(judge(&Verdict::Clean).is_none());
    }

    #[test]
    fn a_scanner_that_could_not_answer_does_not_fail_the_download() {
        // The rule the module exists for. Defender may be absent, replaced,
        // switched off by policy or just slow, and none of those is a
        // statement about the file.
        assert!(judge(&Verdict::Unknown { why: "no scanner".into() }).is_none());
        assert!(judge(&Verdict::Unknown { why: "the scan took too long".into() }).is_none());
    }

    #[test]
    fn a_detection_fails_the_download_and_names_what_was_found() {
        let failure =
            judge(&Verdict::Infected { name: "Trojan:Win32/Wacatac.B!ml".into() }).unwrap();
        assert_eq!(failure.kind, Kind::ContentInfected);
        assert!(failure.summary.contains("Trojan:Win32/Wacatac.B!ml"), "{}", failure.summary);
        assert!(!failure.transient, "downloading it again would find the same thing");
    }

    #[test]
    fn the_detail_says_the_file_is_still_there() {
        // Somebody reading this has a file on their disk that has just been
        // called malware. That it was not deleted is the thing they need to
        // know, and it is not obvious.
        let failure = judge(&Verdict::Infected { name: "X".into() }).unwrap();
        let detail = failure.detail.to_lowercase();
        assert!(detail.contains("nothing has been deleted"), "{detail}");
        assert!(detail.contains("still"), "{detail}");
    }

    #[tokio::test]
    async fn nothing_is_scanned_when_scanning_is_switched_off() {
        // Including on a path that does not exist, which is how this test
        // knows no scanner was reached.
        let settings = Settings { scan_downloads: false, ..Settings::default() };
        let (report, refusal) = run(&settings, Path::new("Z:/not/here.bin")).await;
        assert_eq!(report, Report::default());
        assert!(refusal.is_none());
    }
}
