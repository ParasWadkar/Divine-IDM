//! Sorting finished downloads into folders by what they are.
//!
//! The classification is by file extension and nothing else. A server's
//! `Content-Type` is wrong often enough to be useless for this — plenty
//! serve everything as `application/octet-stream` — and sniffing the bytes
//! would mean waiting for some of the file before knowing where to put it,
//! which is the one thing the writer cannot do: the destination has to be
//! settled before the first byte is written.
//!
//! The folder names come from this fixed list, never from the server or the
//! URL. That is what makes the result safe to join onto the output
//! directory: a name taken from a download could contain `..` and climb out
//! of it, and one of these cannot.

/// Where a download belongs, or nothing when it fits no category.
///
/// Returning nothing rather than "Other" is deliberate. A folder of
/// everything unrecognised is a junk drawer nobody opens; leaving those
/// files in the download folder itself puts them where they would have gone
/// anyway, which is where someone will look for them.
pub fn category_for(filename: &str) -> Option<&'static str> {
    let extension =
        filename.rsplit_once('.').map(|(_, extension)| extension.to_ascii_lowercase())?;

    // Longest-lived groupings first: these are the folders people already
    // have, so a download manager should fill those rather than invent new
    // ones beside them.
    const TABLE: &[(&str, &[&str])] = &[
        (
            "Video",
            &[
                "mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "m4v", "mpg", "mpeg", "ts",
                "m2ts", "vob", "ogv", "3gp",
            ],
        ),
        (
            "Audio",
            &["mp3", "flac", "wav", "aac", "m4a", "ogg", "oga", "opus", "wma", "aiff", "ape"],
        ),
        (
            "Documents",
            &[
                "pdf", "doc", "docx", "odt", "rtf", "txt", "xls", "xlsx", "ods", "csv", "ppt",
                "pptx", "odp", "epub", "mobi", "azw3", "djvu",
            ],
        ),
        (
            "Programs",
            &["exe", "msi", "msix", "appx", "dmg", "pkg", "deb", "rpm", "appimage", "apk", "jar"],
        ),
        (
            "Archives",
            &["zip", "rar", "7z", "tar", "gz", "bz2", "xz", "zst", "tgz", "iso", "img", "cab"],
        ),
        (
            "Images",
            &["jpg", "jpeg", "png", "gif", "webp", "bmp", "tiff", "tif", "svg", "heic", "avif"],
        ),
    ];

    TABLE
        .iter()
        .find_map(|(name, extensions)| extensions.contains(&extension.as_str()).then_some(*name))
}

/// Every folder this can produce.
///
/// Exposed so a caller can create them, or show the user what sorting will
/// do, without having to know the table.
pub fn all_categories() -> &'static [&'static str] {
    &["Video", "Audio", "Documents", "Programs", "Archives", "Images"]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_downloads_land_where_they_belong() {
        assert_eq!(category_for("holiday.mp4"), Some("Video"));
        assert_eq!(category_for("album.flac"), Some("Audio"));
        assert_eq!(category_for("manual.pdf"), Some("Documents"));
        assert_eq!(category_for("setup.exe"), Some("Programs"));
        assert_eq!(category_for("backup.tar.gz"), Some("Archives"));
        assert_eq!(category_for("photo.JPEG"), Some("Images"));
    }

    #[test]
    fn the_extension_is_read_without_regard_to_case() {
        // Servers serve .MP4 and .Mp4; they are the same file.
        for name in ["a.MP4", "a.Mp4", "a.mP4"] {
            assert_eq!(category_for(name), Some("Video"), "{name}");
        }
    }

    #[test]
    fn only_the_last_extension_counts() {
        // `backup.tar.gz` is an archive; `report.pdf.exe` is a program, and
        // calling it a document because of the middle extension is exactly
        // how that trick works.
        assert_eq!(category_for("report.pdf.exe"), Some("Programs"));
        assert_eq!(category_for("video.mp4.zip"), Some("Archives"));
    }

    #[test]
    fn something_unrecognised_gets_no_folder_rather_than_a_junk_one() {
        // It stays in the download folder, which is where someone will look.
        assert_eq!(category_for("data.bin"), None);
        assert_eq!(category_for("firmware.rom"), None);
        assert_eq!(category_for("no-extension"), None);
        assert_eq!(category_for(""), None);
        assert_eq!(category_for(".hidden"), None, "a leading dot is not an extension");
    }

    #[test]
    fn a_folder_name_can_never_come_from_the_download() {
        // The whole reason the table is fixed: these names are joined onto
        // the output directory, and a name taken from a URL or a server
        // could climb out of it.
        for hostile in [
            "a.../../../windows/system32",
            "a.exe/../../..",
            "x.mp4\\..\\..\\secrets",
            "y.%2e%2e%2fpdf",
        ] {
            if let Some(category) = category_for(hostile) {
                assert!(all_categories().contains(&category), "{category} is not from the table");
                assert!(!category.contains(['/', '\\', '.']), "{category}");
            }
        }
    }

    #[test]
    fn every_category_the_table_produces_is_listed() {
        // all_categories is used to pre-create folders; one missing from it
        // would be a folder created lazily and inconsistently.
        for name in ["a.mp4", "a.mp3", "a.pdf", "a.exe", "a.zip", "a.png"] {
            let category = category_for(name).unwrap();
            assert!(all_categories().contains(&category), "{category} missing from the list");
        }
    }

    #[test]
    fn a_dotted_name_with_no_real_extension_is_not_classified() {
        assert_eq!(category_for("version.1.2.3"), None);
    }
}
