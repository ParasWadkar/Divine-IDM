//! Durable identity of an open file.
//!
//! The resume journal records the identity of the `.part` file it describes.
//! On recovery, a mismatch means the file is not the one the journal was
//! written for — it was replaced, restored from backup, copied in from
//! elsewhere, or recreated by an antivirus product. The extent map would then
//! describe bytes that are not there, and resuming would splice unrelated
//! content into the output.
//!
//! A path is not an identity and neither is a size, so this asks the
//! filesystem: `(volume serial, 128-bit file id)` is stable across renames and
//! unique within a volume.

use std::fs::File;
use std::io;

/// A filesystem-assigned identity for a file, stable across renames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    pub volume_serial: u64,
    pub file_id: [u8; 16],
}

impl FileIdentity {
    /// Serialises to the 24 bytes stored in the journal header.
    pub fn to_bytes(self) -> [u8; 24] {
        let mut bytes = [0u8; 24];
        bytes[..8].copy_from_slice(&self.volume_serial.to_le_bytes());
        bytes[8..].copy_from_slice(&self.file_id);
        bytes
    }

    pub fn from_bytes(bytes: [u8; 24]) -> Self {
        let mut volume = [0u8; 8];
        volume.copy_from_slice(&bytes[..8]);
        let mut file_id = [0u8; 16];
        file_id.copy_from_slice(&bytes[8..]);
        Self { volume_serial: u64::from_le_bytes(volume), file_id }
    }

    /// Reads the identity of an open file.
    ///
    /// Both ReFS and NTFS support the 128-bit id. On a filesystem that does
    /// not (some network redirectors), this returns an error and the caller
    /// should fall back to verifying content with the journal's block CRCs,
    /// which is the stronger check anyway.
    pub fn of(file: &File) -> io::Result<Self> {
        platform::file_identity(file)
    }
}

#[cfg(windows)]
mod platform {
    use super::FileIdentity;
    use std::fs::File;
    use std::io;
    use std::os::windows::io::AsRawHandle;

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
    };

    pub(super) fn file_identity(file: &File) -> io::Result<FileIdentity> {
        let handle = HANDLE(file.as_raw_handle());
        let mut info = FILE_ID_INFO::default();

        // SAFETY: `handle` is a valid file handle borrowed from `file`, which
        // outlives this call. `info` is a correctly sized, properly aligned
        // FILE_ID_INFO, and the size passed matches it exactly, so the kernel
        // cannot write out of bounds.
        unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileIdInfo,
                (&raw mut info).cast(),
                size_of::<FILE_ID_INFO>() as u32,
            )
        }
        .map_err(|error| io::Error::other(format!("GetFileInformationByHandleEx: {error}")))?;

        Ok(FileIdentity { volume_serial: info.VolumeSerialNumber, file_id: info.FileId.Identifier })
    }
}

#[cfg(not(windows))]
mod platform {
    use super::FileIdentity;
    use std::fs::File;
    use std::io;

    /// Non-Windows exists only so the crate's logic can be unit-tested on CI
    /// runners other than Windows; the shipping engine is Windows-only.
    pub(super) fn file_identity(file: &File) -> io::Result<FileIdentity> {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        let mut file_id = [0u8; 16];
        file_id[..8].copy_from_slice(&metadata.ino().to_le_bytes());
        Ok(FileIdentity { volume_serial: metadata.dev(), file_id })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn identity_round_trips_through_bytes() {
        let identity = FileIdentity { volume_serial: 0x0123_4567_89ab_cdef, file_id: [7u8; 16] };
        assert_eq!(FileIdentity::from_bytes(identity.to_bytes()), identity);
    }

    #[test]
    fn identity_is_stable_across_reopen_and_rename() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("part.idmpart");
        let renamed = dir.path().join("final.bin");

        let mut file = File::create(&original).unwrap();
        file.write_all(b"payload").unwrap();
        let first = FileIdentity::of(&file).unwrap();
        drop(file);

        // Reopening the same path must yield the same identity.
        let reopened = File::open(&original).unwrap();
        assert_eq!(FileIdentity::of(&reopened).unwrap(), first);
        drop(reopened);

        // So must renaming it: identity follows the file, not the path. This is
        // what lets the journal survive the final rename.
        std::fs::rename(&original, &renamed).unwrap();
        let after_rename = File::open(&renamed).unwrap();
        assert_eq!(FileIdentity::of(&after_rename).unwrap(), first);
    }

    #[test]
    fn a_different_file_has_a_different_identity() {
        let dir = tempfile::tempdir().unwrap();
        let first_path = dir.path().join("a.bin");
        let second_path = dir.path().join("b.bin");

        let first = File::create(&first_path).unwrap();
        let second = File::create(&second_path).unwrap();
        assert_ne!(FileIdentity::of(&first).unwrap(), FileIdentity::of(&second).unwrap());
    }

    #[test]
    fn replacing_a_file_at_the_same_path_changes_its_identity() {
        // The case this check exists for: the journal describes bytes in a file
        // that has since been swapped out from under it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("part.idmpart");

        let file = File::create(&path).unwrap();
        let before = FileIdentity::of(&file).unwrap();
        drop(file);

        std::fs::remove_file(&path).unwrap();
        let recreated = File::create(&path).unwrap();
        let after = FileIdentity::of(&recreated).unwrap();

        assert_ne!(before, after, "a recreated file must not pass as the original");
    }
}
