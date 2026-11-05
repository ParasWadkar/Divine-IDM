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

use std::path::{Path, PathBuf};

use idm_ipc::Settings;

use crate::failure::{Failure, Kind};
use crate::hook::{self, Ran};
use crate::scan::{self, Verdict};
use crate::unpack::{self, Unpacked};

/// What the steps turned up, for the record rather than for a decision.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// `None` when scanning was switched off or no scanner was found.
    pub scan: Option<Verdict>,
    /// `None` when no hook is set, or the one set was refused.
    pub hook: Option<Ran>,
    /// `None` when unpacking is switched off or the file is not a zip.
    pub unpacked: Option<Unpacked>,
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
/// `default_dir` is the folder the engine was started with, which together
/// with the chosen one is where downloads can land — the two places a hook
/// program may not live in.
pub async fn run(
    settings: &Settings,
    default_dir: &Path,
    file: &Path,
) -> (Report, Option<Failure>) {
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

    // Before the hook, so a hook that wants to look at what was in the
    // archive finds it there.
    if settings.extract_zips && unpack::looks_like_a_zip(file) {
        report.unpacked = Some(unpack::unpack(file).await);
    }

    report.hook = run_hook(settings, default_dir, file).await;

    (report, None)
}

/// Runs the user's hook, if there is one that still passes its checks.
///
/// Vetted again here, not only when it was chosen. The download folder can
/// be changed afterwards to the folder the hook lives in, which would
/// otherwise leave a program that one download can replace and the next
/// download runs.
async fn run_hook(settings: &Settings, default_dir: &Path, file: &Path) -> Option<Ran> {
    if settings.hook_program.trim().is_empty() {
        return None;
    }

    let downloads = vec![PathBuf::from(&settings.download_dir), default_dir.to_path_buf()];
    let program = match hook::vet(&settings.hook_program, &downloads) {
        Ok(program) if !program.is_empty() => PathBuf::from(program),
        // Reported as not having run rather than as a failed download: the
        // file is fine, and the thing that is wrong is a setting.
        Ok(_) => return None,
        Err(why) => return Some(Ran::Never { why }),
    };

    let arguments = match hook::vet_arguments(&settings.hook_arguments) {
        Ok(arguments) => hook::fill(&arguments, file),
        Err(why) => return Some(Ran::Never { why }),
    };

    Some(hook::run(&program, &arguments).await)
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
        let (report, refusal) =
            run(&settings, Path::new("Z:/downloads"), Path::new("Z:/not/here.bin")).await;
        assert_eq!(report, Report::default());
        assert!(refusal.is_none());
    }

    #[tokio::test]
    async fn no_hook_means_nothing_is_run() {
        let settings = Settings { scan_downloads: false, ..Settings::default() };
        let (report, _) =
            run(&settings, Path::new("Z:/downloads"), Path::new("Z:/downloads/x.bin")).await;
        assert!(report.hook.is_none());
    }

    #[tokio::test]
    async fn a_hook_sitting_in_the_download_folder_is_not_run() {
        // The check is repeated here, not only where the hook was chosen,
        // because the download folder can be changed to the hook's folder
        // afterwards -- at which point one download could replace the
        // program and the next download would run it.
        let dir = tempfile::tempdir().unwrap();
        let downloads = dir.path().join("Downloads");
        std::fs::create_dir_all(&downloads).unwrap();
        let program = downloads.join("after.exe");
        std::fs::write(&program, b"MZ").unwrap();

        let settings = Settings {
            scan_downloads: false,
            download_dir: downloads.display().to_string(),
            hook_program: program.display().to_string(),
            ..Settings::default()
        };
        let (report, refusal) = run(&settings, dir.path(), &downloads.join("finished.bin")).await;

        assert!(refusal.is_none(), "a bad hook is a bad setting, not a bad download");
        match report.hook {
            Some(Ran::Never { why }) => assert!(why.contains("downloads are saved"), "{why}"),
            other => panic!("it must not have been run: {other:?}"),
        }
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn a_hook_that_runs_is_recorded_and_changes_nothing() {
        // `where.exe` is in System32 on every Windows, takes an argument,
        // and exits quickly either way -- so it proves the launch without
        // the test needing to build a program of its own.
        let system32 = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32");
        let program = system32.join("where.exe");
        if !program.is_file() {
            eprintln!("no where.exe on this machine; skipping");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("finished.bin");
        std::fs::write(&file, b"x").unwrap();

        let settings = Settings {
            scan_downloads: false,
            hook_program: program.display().to_string(),
            hook_arguments: vec!["{path}".to_owned()],
            ..Settings::default()
        };
        let (report, refusal) = run(&settings, dir.path(), &file).await;

        assert!(refusal.is_none(), "a hook must never be able to fail a download");
        assert!(matches!(report.hook, Some(Ran::Finished { .. })), "{:?}", report.hook);
    }

    #[tokio::test]
    async fn a_zip_is_left_alone_unless_unpacking_was_asked_for() {
        // Off by default, and this is the test that keeps it that way:
        // writing files into somebody's Downloads folder unasked is the
        // kind of help that is hard to undo.
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("pack.zip");
        std::fs::write(&archive, b"not really a zip").unwrap();

        let settings = Settings { scan_downloads: false, ..Settings::default() };
        assert!(!settings.extract_zips);
        let (report, _) = run(&settings, dir.path(), &archive).await;
        assert!(report.unpacked.is_none());
        assert!(!dir.path().join("pack").exists());
    }

    #[tokio::test]
    async fn only_a_zip_is_unpacked() {
        let dir = tempfile::tempdir().unwrap();
        let other = dir.path().join("setup.bin");
        std::fs::write(&other, b"x").unwrap();

        let settings =
            Settings { scan_downloads: false, extract_zips: true, ..Settings::default() };
        let (report, _) = run(&settings, dir.path(), &other).await;
        assert!(report.unpacked.is_none(), "{:?}", report.unpacked);
    }

    #[tokio::test]
    async fn a_zip_that_cannot_be_unpacked_still_delivers_the_download() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("pack.zip");
        std::fs::write(&archive, b"not really a zip").unwrap();

        let settings =
            Settings { scan_downloads: false, extract_zips: true, ..Settings::default() };
        let (report, refusal) = run(&settings, dir.path(), &archive).await;
        assert!(refusal.is_none(), "unpacking must never fail a download");
        assert!(matches!(report.unpacked, Some(crate::unpack::Unpacked::Not { .. })));
    }

    #[tokio::test]
    async fn a_hook_that_is_not_there_does_not_fail_the_download() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings {
            scan_downloads: false,
            hook_program: dir.path().join("gone.exe").display().to_string(),
            ..Settings::default()
        };
        let (report, refusal) = run(&settings, dir.path(), &dir.path().join("x.bin")).await;
        assert!(refusal.is_none());
        assert!(matches!(report.hook, Some(Ran::Never { .. })));
    }
}
