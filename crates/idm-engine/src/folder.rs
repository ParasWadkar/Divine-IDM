//! Vetting the folder a user chooses for downloads.
//!
//! Only the app and the command line can set it, so this is not defending
//! against a hostile caller; the engine already refuses settings changes from
//! the browser side. It is defending against a folder that looks fine when
//! picked and fails later, which for a download manager is the worst place:
//! the failure arrives mid-transfer, as a write error, on a file that may
//! have taken an hour to fetch. So everything that can be learned about the
//! folder up front is learned here, when the person is looking at the
//! screen and can still fix it.

use std::path::{Component, Path};

/// What a chosen folder has to be, checked, or the reason it is not.
///
/// Returns the path to store. An empty string is valid and means "the
/// default", so it passes through untouched.
pub fn vet_download_dir(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(String::new());
    }

    // Verbatim and device paths (`\\?\`, `\\.\`) skip the checks Windows
    // normally applies to a path, and nobody picking a folder means one.
    if raw.starts_with(r"\\.\") || raw.starts_with(r"\\?\") {
        return Err("that is a device path, not a folder; choose an ordinary folder".into());
    }

    let path = Path::new(raw);

    // Absolute, because a relative folder means whatever the working
    // directory happens to be, and for a background app that is
    // System32 or wherever the shortcut was launched from. `C:foo` and
    // `\foo` are rooted on one side only and count as relative here.
    if !path.is_absolute() {
        return Err("the folder has to be a full path, such as D:\\Downloads".into());
    }

    // No `..`: the stored path is shown back to the person and compared
    // against others, and a path that climbs is neither of those things
    // reliably.
    if path.components().any(|part| matches!(part, Component::ParentDir)) {
        return Err("the folder cannot contain `..`; write out the full path".into());
    }

    if path.exists() && !path.is_dir() {
        return Err("that path is a file, not a folder".into());
    }

    // Created if missing: choosing `D:\Downloads` before it exists is
    // ordinary, and refusing would send the person to Explorer and back.
    std::fs::create_dir_all(path).map_err(|error| format!("could not create the folder: {error}"))?;

    // The only reliable test of "can I write here" is writing. Metadata
    // reports a read-only flag that Windows ignores for directories, and
    // says nothing of quotas, permissions or a share gone read-only.
    let probe = path.join(format!(".idm-write-test-{}", std::process::id()));
    std::fs::write(&probe, b"")
        .map_err(|error| format!("could not write to that folder: {error}"))?;
    let _ = std::fs::remove_file(&probe);

    // A trailing separator would make `D:\x\` and `D:\x` look like two
    // different folders to anything comparing the strings.
    let trimmed = raw.trim_end_matches(['\\', '/']);
    // But never strip a drive root down to `D:`, which means "the current
    // directory on D:" rather than the root.
    let stored = if trimmed.len() == 2 && trimmed.ends_with(':') {
        format!("{trimmed}\\")
    } else {
        trimmed.to_owned()
    };
    Ok(stored)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_means_the_default_and_is_always_accepted() {
        assert_eq!(vet_download_dir("").unwrap(), "");
        assert_eq!(vet_download_dir("   ").unwrap(), "");
    }

    #[test]
    fn an_existing_writable_folder_is_accepted_as_written() {
        let dir = tempfile::tempdir().unwrap();
        let text = dir.path().display().to_string();
        assert_eq!(vet_download_dir(&text).unwrap(), text);
    }

    #[test]
    fn a_missing_folder_is_created_including_its_parents() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("Downloads").join("Fresh");
        assert!(!target.exists());
        vet_download_dir(&target.display().to_string()).unwrap();
        assert!(target.is_dir(), "choosing a folder that does not exist yet should make it");
    }

    #[test]
    fn the_write_probe_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        vet_download_dir(&dir.path().display().to_string()).unwrap();
        let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert!(left.is_empty(), "a probe file was left in the user's folder");
    }

    #[test]
    fn a_relative_path_is_refused_because_it_means_the_working_directory() {
        for relative in ["Downloads", r"..\x", ".", r"sub\dir", "C:drive-relative", r"\rooted"] {
            let error = vet_download_dir(relative).unwrap_err();
            assert!(error.contains("full path"), "{relative}: {error}");
        }
    }

    #[test]
    fn parent_components_are_refused_even_inside_an_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let sneaky = dir.path().join("a").join("..").join("b");
        let error = vet_download_dir(&sneaky.display().to_string()).unwrap_err();
        assert!(error.contains(".."), "{error}");
    }

    #[test]
    fn a_file_is_not_a_folder() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, b"x").unwrap();
        let error = vet_download_dir(&file.display().to_string()).unwrap_err();
        assert!(error.contains("file"), "{error}");
    }

    #[test]
    fn device_and_verbatim_paths_are_refused() {
        for path in [r"\\.\C:\x", r"\\?\C:\x", r"\\.\pipe\idm"] {
            assert!(vet_download_dir(path).is_err(), "{path}");
        }
    }

    #[test]
    fn a_trailing_separator_is_not_stored() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().display().to_string();
        let with_slash = format!("{plain}\\");
        assert_eq!(vet_download_dir(&with_slash).unwrap(), plain);
    }

    #[test]
    fn surrounding_whitespace_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().display().to_string();
        assert_eq!(vet_download_dir(&format!("  {plain}  ")).unwrap(), plain);
    }

    #[test]
    fn a_path_that_cannot_be_created_is_refused_with_the_reason() {
        // A path whose parent is a file can never be created, on any
        // platform, whoever is running the tests.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("blocker");
        std::fs::write(&file, b"x").unwrap();
        let impossible = file.join("child");
        let error = vet_download_dir(&impossible.display().to_string()).unwrap_err();
        assert!(error.contains("could not"), "{error}");
    }
}
