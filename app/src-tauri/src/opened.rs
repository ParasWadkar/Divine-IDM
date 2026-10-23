//! Files the shell hands to the app: a double-clicked `.torrent`.
//!
//! When Windows opens a file with a program it starts that program with the
//! file's path as an argument. For this app that arrives in two ways: as the
//! arguments of a fresh start, or -- when the app is already running in the
//! tray, which is the usual case -- as the arguments of a second launch that
//! the single-instance plugin forwards and then discards. Both go through
//! [`torrent_files`] so they cannot disagree about what counts.
//!
//! The paths are only ever reported, never opened. There is no torrent engine
//! behind them yet, and a path taken from a command line is not something to
//! start reading files from without a reason.

use std::path::Path;

/// How many files one launch may name. Selecting forty torrents in Explorer
/// and pressing Enter does pass forty paths, and that is fine; a command line
/// built to be enormous is not.
const MAX_FILES: usize = 64;

/// Longest path accepted. Windows allows more, but nothing real is this long
/// and the value is shown to the user.
const MAX_PATH_CHARS: usize = 1024;

/// The `.torrent` files among a launch's arguments.
///
/// Matched on the extension, case-insensitively -- `.TORRENT` is what some
/// downloaders produce -- and only if the path is an existing file. The
/// executable's own path is in the argument list too and is dropped by the
/// extension check, which is simpler than counting arguments and cannot go
/// wrong if the shell ever passes them differently.
pub fn torrent_files<I, S>(args: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    args.into_iter()
        .map(|arg| arg.as_ref().trim().trim_matches('"').to_owned())
        .filter(|arg| !arg.is_empty() && arg.chars().count() <= MAX_PATH_CHARS)
        .filter(|arg| {
            Path::new(arg)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("torrent"))
        })
        .filter(|arg| Path::new(arg).is_file())
        .take(MAX_FILES)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(dir: &Path, name: &str) -> String {
        let path = dir.join(name);
        std::fs::write(&path, b"d8:announce0:e").unwrap();
        path.display().to_string()
    }

    #[test]
    fn a_torrent_among_the_arguments_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let torrent = file(dir.path(), "ubuntu.torrent");
        let found = torrent_files(["C:\\app\\divine-idm.exe".to_owned(), torrent.clone()]);
        assert_eq!(found, vec![torrent]);
    }

    #[test]
    fn the_extension_is_matched_without_regard_to_case() {
        let dir = tempfile::tempdir().unwrap();
        let upper = file(dir.path(), "SHOUT.TORRENT");
        assert_eq!(torrent_files([upper.clone()]), vec![upper]);
    }

    #[test]
    fn other_files_and_the_executable_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let text = file(dir.path(), "notes.txt");
        let found = torrent_files(["C:\\app\\divine-idm.exe".to_owned(), text, "--hidden".into()]);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_torrent_that_does_not_exist_is_ignored() {
        // Nothing is shown for a path that is not there; a stale shortcut or a
        // mistyped argument is not worth a notice.
        assert!(torrent_files(["C:\\nowhere\\ghost.torrent"]).is_empty());
    }

    #[test]
    fn a_directory_named_like_a_torrent_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("trick.torrent");
        std::fs::create_dir(&folder).unwrap();
        assert!(torrent_files([folder.display().to_string()]).is_empty());
    }

    #[test]
    fn quotes_around_a_path_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let torrent = file(dir.path(), "quoted.torrent");
        assert_eq!(torrent_files([format!("\"{torrent}\"")]), vec![torrent]);
    }

    #[test]
    fn a_launch_naming_far_too_many_is_cut_off() {
        let dir = tempfile::tempdir().unwrap();
        let paths: Vec<String> =
            (0..MAX_FILES + 20).map(|n| file(dir.path(), &format!("t{n}.torrent"))).collect();
        assert_eq!(torrent_files(paths).len(), MAX_FILES);
    }

    #[test]
    fn an_absurdly_long_path_is_ignored() {
        let long = format!("C:\\{}.torrent", "a".repeat(MAX_PATH_CHARS));
        assert!(torrent_files([long]).is_empty());
    }

    #[test]
    fn nothing_at_all_is_fine() {
        assert!(torrent_files(Vec::<String>::new()).is_empty());
        assert!(torrent_files([""]).is_empty());
    }
}
