//! Running a program of the user's choosing when a download finishes.
//!
//! The feature every download manager has, and the one with the largest
//! gap between how simple it looks and how badly it can go. What follows is
//! mostly the rules that close that gap, because the running part is six
//! lines.
//!
//! # There is no shell
//!
//! The obvious implementation is to keep a command line and hand it to
//! `cmd /c`. That is also how this becomes remote code execution: the
//! program is given the *file name the server chose*, and a server that
//! names a file `x & calc.exe` has then run a command on the machine. There
//! is no amount of quoting that reliably prevents it, because `cmd.exe`
//! re-parses its argument after the quotes have been removed.
//!
//! So a hook is a program and a list of arguments, each argument passed as
//! one argument, and no interpreter is involved. A file name containing
//! `&`, `|`, `%` or a quote arrives at the program as those characters,
//! which is what it should be.
//!
//! # `.exe` only
//!
//! A `.bat` or `.cmd` file is run *by* `cmd.exe`, so allowing one puts the
//! parser straight back in the path — and `CreateProcess` will happily
//! launch one. `.ps1`, `.vbs` and `.js` are the same with a different
//! interpreter, and a `.lnk` is a pointer to anything at all. An `.exe` is
//! the one case where the arguments this module built are the arguments the
//! program receives.
//!
//! # Never a program inside a download folder
//!
//! The hook is chosen once and runs after every download. If it could live
//! in the folder downloads land in, then one download could replace the
//! program that the next download runs, and the user would have authorised
//! none of it. Checked against the download folders at the moment the hook
//! is chosen *and* refused again before each run, because the folder can
//! change afterwards.
//!
//! # A hook never fails a download
//!
//! Same rule as the virus check, for the same reason: a hook that is
//! missing, slow or unhappy has said nothing about the file. The exit code
//! is recorded and that is all it does.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

/// How long a hook may run before it is abandoned.
///
/// A hook is meant to notify something or kick off a job, not to be the
/// job. One that has hung must not hold the pipeline open behind it, and
/// killing it is safe in a way that killing a download is not — nothing of
/// ours is half-written.
pub const LONGEST_HOOK: Duration = Duration::from_secs(60);

/// How many arguments a hook may be given.
pub const MOST_ARGUMENTS: usize = 16;

/// How long one argument may be.
///
/// Comfortably inside the command-line limit even at the full count, so a
/// hook can never be configured into a launch that fails for a reason the
/// user cannot see.
pub const LONGEST_ARGUMENT: usize = 1024;

/// What happened when the hook was run.
///
/// Recorded, never acted on. See the module docs: an exit code from a
/// program the user chose is not a verdict on the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ran {
    /// It ran to the end. A non-zero code is still this.
    Finished { code: i32 },
    /// It could not be run, or did not finish, and why.
    Never { why: String },
}

/// Checks a chosen program and returns it tidied, or says why not.
///
/// Empty means no hook, which is not an error. Everything else is checked
/// here rather than at run time so that a mistake is reported while the
/// person is still looking at the control that caused it.
pub fn vet(program: &str, downloads: &[PathBuf]) -> Result<String, String> {
    let raw = program.trim();
    if raw.is_empty() {
        return Ok(String::new());
    }

    // Verbatim and device paths skip the normalisation Windows applies to
    // every other path, which would make every comparison below unreliable.
    if raw.starts_with(r"\\.\") || raw.starts_with(r"\\?\") {
        return Err("that is a device path, not a program; choose an ordinary .exe".into());
    }

    let path = Path::new(raw);

    if !path.is_absolute() {
        return Err("the program has to be a full path, such as C:\\Tools\\notify.exe".into());
    }

    if path.components().any(|part| matches!(part, Component::ParentDir)) {
        return Err("the path cannot contain `..`; write out the full path".into());
    }

    // The one extension where the arguments built here are the arguments the
    // program is handed. See the module docs.
    let extension = path.extension().and_then(|part| part.to_str()).unwrap_or_default();
    if !extension.eq_ignore_ascii_case("exe") {
        return Err(
            "only a .exe can be run; a .bat, .cmd, .ps1 or .lnk is run by something else that \
             would read the file name as part of a command"
                .into(),
        );
    }

    if !path.is_file() {
        return Err("there is no program at that path".into());
    }

    if let Some(folder) = inside_any(path, downloads) {
        return Err(format!(
            "that program is inside {}, where downloads are saved: a download could replace it \
             and then be run. Keep the hook somewhere downloads cannot reach.",
            folder.display()
        ));
    }

    Ok(raw.to_owned())
}

/// Everywhere a download can land, which is where a hook may not live.
///
/// One function rather than the pair of paths assembled at each call site.
/// It is a security-relevant definition — "the folders a download could
/// write into" — and two hand-built copies of it would be two chances to
/// forget one when a third place to land is added.
pub fn landing_dirs(chosen: &str, default_dir: &Path) -> Vec<PathBuf> {
    vec![PathBuf::from(chosen), default_dir.to_path_buf()]
}

/// The download folder a path sits in, if it sits in one.
///
/// Canonicalised on both sides where possible, because `D:\dl`, `d:\DL` and
/// `D:\dl\.` are the same folder and a textual comparison says they are
/// three. A folder that cannot be canonicalised — one that does not exist
/// yet — is still compared as written, since the answer "not inside it" for
/// a folder nobody can resolve would be the unsafe direction.
pub fn inside_any(program: &Path, downloads: &[PathBuf]) -> Option<PathBuf> {
    let settled = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let program = settled(program);

    downloads
        .iter()
        .filter(|folder| !folder.as_os_str().is_empty())
        .find(|folder| program.starts_with(settled(folder)))
        .cloned()
}

/// Checks the argument list, returning it trimmed of empties.
///
/// Capped in both directions. Not because a long list is dangerous — there
/// is no shell to confuse — but because the whole thing is written to
/// `settings.json`, and a file the program rewrites on every save should
/// have a bound.
pub fn vet_arguments(arguments: &[String]) -> Result<Vec<String>, String> {
    if arguments.len() > MOST_ARGUMENTS {
        return Err(format!("a hook can be given at most {MOST_ARGUMENTS} arguments"));
    }
    if let Some(long) = arguments.iter().find(|argument| argument.len() > LONGEST_ARGUMENT) {
        return Err(format!(
            "one of the arguments is {} characters; the limit is {LONGEST_ARGUMENT}",
            long.len()
        ));
    }
    // A NUL cannot survive the trip to `CreateProcess`, which would fail the
    // launch with an error that says nothing about which argument did it.
    if arguments.iter().any(|argument| argument.contains('\0')) {
        return Err("an argument contains a zero byte".into());
    }
    Ok(arguments.iter().filter(|argument| !argument.trim().is_empty()).cloned().collect())
}

/// Puts the finished file into the argument list.
///
/// Three placeholders, substituted anywhere in an argument rather than only
/// as a whole one, so `--file={path}` works. Safe to substitute mid-argument
/// precisely because nothing re-parses the result: there is no shell, so the
/// text goes to the program exactly as assembled.
///
/// An unrecognised `{...}` is left alone. A hook is somebody else's program
/// and may have a syntax of its own.
pub fn fill(arguments: &[String], file: &Path) -> Vec<String> {
    let path = file.display().to_string();
    let name = file.file_name().map(|part| part.to_string_lossy().into_owned()).unwrap_or_default();
    let folder =
        file.parent().map(|part| part.display().to_string()).unwrap_or_else(|| path.clone());

    arguments
        .iter()
        .map(|argument| {
            argument.replace("{path}", &path).replace("{name}", &name).replace("{folder}", &folder)
        })
        .collect()
}

/// Runs the hook and says what happened.
pub async fn run(program: &Path, arguments: &[String]) -> Ran {
    let mut command = tokio::process::Command::new(program);
    command.args(arguments);
    // Nothing to read, and a hook that waited on stdin would otherwise hang
    // until the timeout rather than failing at once.
    command.stdin(std::process::Stdio::null());
    // Discarded rather than inherited: this process has no console, and
    // leaving the pipes open would mean a chatty hook filling a buffer
    // nobody drains and blocking on its own output.
    command.stdout(std::process::Stdio::null());
    command.stderr(std::process::Stdio::null());
    // The program's own folder, not ours. Ours is wherever the app was
    // launched from, which for a startup entry is System32 — a surprising
    // place for a hook to resolve a relative path against.
    if let Some(folder) = program.parent() {
        command.current_dir(folder);
    }

    #[cfg(windows)]
    {
        // A hook is a background errand. A console window flashing up after
        // every download would be the most visible thing about the feature.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let mut started = match command.spawn() {
        Ok(child) => child,
        Err(error) => return Ran::Never { why: error.to_string() },
    };

    match tokio::time::timeout(LONGEST_HOOK, started.wait()).await {
        Ok(Ok(status)) => match status.code() {
            Some(code) => Ran::Finished { code },
            None => Ran::Never { why: "it was stopped before it finished".into() },
        },
        Ok(Err(error)) => Ran::Never { why: error.to_string() },
        Err(_) => {
            // Killed, deliberately. A hook holding the pipeline open past
            // its budget is a bug in the hook, and leaving it running would
            // mean one per download for as long as the app is up.
            let _ = started.start_kill();
            Ran::Never { why: format!("it ran longer than {} seconds", LONGEST_HOOK.as_secs()) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real file, because `vet` checks the program is there.
    fn an_exe(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"MZ").unwrap();
        path
    }

    #[test]
    fn no_hook_is_not_an_error() {
        assert_eq!(vet("", &[]).unwrap(), "");
        assert_eq!(vet("   ", &[]).unwrap(), "");
    }

    #[test]
    fn a_plain_exe_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let exe = an_exe(dir.path(), "notify.exe");
        let vetted = vet(&exe.display().to_string(), &[]).unwrap();
        assert_eq!(Path::new(&vetted), exe);
    }

    #[test]
    fn a_relative_path_is_refused() {
        let why = vet(r"tools\notify.exe", &[]).unwrap_err();
        assert!(why.contains("full path"), "{why}");
    }

    #[test]
    fn a_path_that_climbs_is_refused() {
        let why = vet(r"C:\Tools\..\Windows\System32\cmd.exe", &[]).unwrap_err();
        assert!(why.contains(".."), "{why}");
    }

    #[test]
    fn a_batch_file_is_refused() {
        // The one that matters. `CreateProcess` would launch it, `cmd.exe`
        // would parse it, and the file name a server chose would be read as
        // part of a command.
        let dir = tempfile::tempdir().unwrap();
        for name in ["after.bat", "after.cmd", "after.ps1", "after.vbs", "after.lnk", "after"] {
            let path = an_exe(dir.path(), name);
            let why = vet(&path.display().to_string(), &[]).unwrap_err();
            assert!(why.contains(".exe"), "{name}: {why}");
        }
    }

    #[test]
    fn a_device_path_is_refused() {
        let why = vet(r"\\?\C:\Tools\notify.exe", &[]).unwrap_err();
        assert!(why.contains("device path"), "{why}");
    }

    #[test]
    fn a_program_that_is_not_there_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let why = vet(&dir.path().join("missing.exe").display().to_string(), &[]).unwrap_err();
        assert!(why.contains("no program"), "{why}");
    }

    #[test]
    fn a_program_inside_a_download_folder_is_refused() {
        // The escalation this check exists for: one download replaces the
        // program, the next download runs it.
        let dir = tempfile::tempdir().unwrap();
        let downloads = dir.path().join("Downloads");
        std::fs::create_dir_all(downloads.join("Video")).unwrap();
        let exe = an_exe(&downloads.join("Video"), "after.exe");

        let folders = std::slice::from_ref(&downloads);
        let why = vet(&exe.display().to_string(), folders).unwrap_err();
        assert!(why.contains("downloads are saved"), "{why}");

        // And the folder itself, not only something under it.
        let top = an_exe(&downloads, "after.exe");
        assert!(vet(&top.display().to_string(), folders).is_err());
    }

    #[test]
    fn a_program_beside_the_download_folder_is_fine() {
        // `D:\DownloadsHelper` is not inside `D:\Downloads`, and a prefix
        // test on the text alone would say it was.
        let dir = tempfile::tempdir().unwrap();
        let downloads = dir.path().join("Downloads");
        std::fs::create_dir_all(&downloads).unwrap();
        let beside = dir.path().join("DownloadsHelper");
        std::fs::create_dir_all(&beside).unwrap();
        let exe = an_exe(&beside, "after.exe");

        assert!(vet(&exe.display().to_string(), &[downloads]).is_ok());
    }

    #[test]
    fn an_empty_download_folder_forbids_nothing() {
        // The setting is empty until somebody chooses one, and an empty path
        // is a prefix of everything.
        let dir = tempfile::tempdir().unwrap();
        let exe = an_exe(dir.path(), "notify.exe");
        assert!(vet(&exe.display().to_string(), &[PathBuf::new()]).is_ok());
    }

    #[test]
    fn the_placeholders_are_filled_in() {
        let arguments = vec![
            "--file={path}".to_owned(),
            "{name}".to_owned(),
            "in {folder}".to_owned(),
            "--flag".to_owned(),
        ];
        let filled = fill(&arguments, Path::new(r"D:\Downloads\Video\clip.mp4"));
        assert_eq!(
            filled,
            vec![
                r"--file=D:\Downloads\Video\clip.mp4",
                "clip.mp4",
                r"in D:\Downloads\Video",
                "--flag",
            ]
        );
    }

    #[test]
    fn a_hostile_file_name_stays_one_argument() {
        // The whole reason there is no shell. These characters are a command
        // separator, a pipe and a variable to `cmd.exe`, and arrive at the
        // program as themselves.
        let filled = fill(&["{name}".to_owned()], Path::new(r"D:\dl\x & calc.exe %PATH% | y.zip"));
        assert_eq!(filled, vec!["x & calc.exe %PATH% | y.zip"]);
    }

    #[test]
    fn a_placeholder_nobody_knows_is_left_alone() {
        let filled = fill(&["{elsewhere}".to_owned()], Path::new(r"D:\dl\x.zip"));
        assert_eq!(filled, vec!["{elsewhere}"]);
    }

    #[test]
    fn too_many_arguments_are_refused() {
        let many = vec!["x".to_owned(); MOST_ARGUMENTS + 1];
        assert!(vet_arguments(&many).is_err());
        assert!(vet_arguments(&many[..MOST_ARGUMENTS]).is_ok());
    }

    #[test]
    fn a_very_long_argument_is_refused() {
        let long = vec!["x".repeat(LONGEST_ARGUMENT + 1)];
        assert!(vet_arguments(&long).is_err());
    }

    #[test]
    fn blank_arguments_are_dropped() {
        // The window sends one argument per line, and a trailing newline is
        // not an empty argument the user meant.
        let given = vec!["--file={path}".to_owned(), "  ".to_owned(), String::new()];
        assert_eq!(vet_arguments(&given).unwrap(), vec!["--file={path}"]);
    }
}
