//! What the destination volume can actually do.
//!
//! The preallocation chain and the writer's shard count both depend on facts
//! that vary per volume and cannot be assumed: whether sparse files are
//! supported (ReFS, exFAT and FAT32 do not), whether a disk quota is actually
//! constraining this user (which can make the *first* write into a sparse hole
//! fail), and whether the device has a seek penalty (more writer shards on a
//! spinning disk is seek thrash, not parallelism).
//!
//! # A trap worth naming
//!
//! `FILE_VOLUME_QUOTAS` means the volume *supports* quotas. NTFS always does,
//! so the flag is set on essentially every Windows volume and says nothing
//! about whether a quota is in force. Reading it as "a quota applies" disables
//! the sparse fast path everywhere, for nothing.
//!
//! The signal that actually means something is behavioural: `GetDiskFreeSpaceEx`
//! reports both the bytes available *to the caller* and the volume's total free
//! bytes. A quota makes the first materially smaller than the second.

use std::io;
use std::path::{Path, PathBuf};

/// A quota has to shave off at least this much before it is treated as real,
/// so that incidental accounting differences do not cost us the sparse path.
const QUOTA_SLACK: u64 = 1 << 20;

/// Capabilities of the volume a path lives on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeCaps {
    /// Root of the volume, e.g. `D:\`.
    pub root: PathBuf,
    pub filesystem: String,
    /// `FSCTL_SET_SPARSE` can succeed here.
    pub supports_sparse: bool,
    /// The volume *supports* quotas. Informational only — NTFS always reports
    /// this, so it must never be used to decide anything. See
    /// [`quota_restricted`](Self::quota_restricted).
    pub supports_quotas: bool,
    /// A quota is genuinely constraining this user: fewer bytes are available
    /// to the caller than the volume has free. Writing into a sparse hole can
    /// then fail with `ERROR_DISK_QUOTA_EXCEEDED`, so the sparse path is
    /// skipped up front rather than discovered mid-download.
    pub quota_restricted: bool,
    /// A local fixed disk. `SetFileValidData` is only ever attempted here.
    pub is_fixed: bool,
}

impl VolumeCaps {
    /// Whether the sparse path should be taken on this volume.
    pub fn should_try_sparse(&self) -> bool {
        self.supports_sparse && !self.quota_restricted
    }
}

/// Free-space figures for a volume. The two differ precisely when a quota
/// applies to the calling user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskSpace {
    pub available_to_caller: u64,
    pub total_free: u64,
    pub total: u64,
}

impl DiskSpace {
    fn quota_restricted(&self) -> bool {
        self.total_free.saturating_sub(self.available_to_caller) > QUOTA_SLACK
    }
}

/// Reads the capabilities of the volume containing `path`.
pub fn caps_for(path: &Path) -> io::Result<VolumeCaps> {
    platform::caps_for(path)
}

pub fn disk_space(path: &Path) -> io::Result<DiskSpace> {
    platform::disk_space(path)
}

/// Bytes available to this user on the volume containing `path`.
///
/// Deliberately the caller-available figure rather than total free space: with
/// a quota in force those differ, and the smaller one is what a write will hit.
pub fn free_space(path: &Path) -> io::Result<u64> {
    Ok(platform::disk_space(path)?.available_to_caller)
}

/// `Some(true)` for a device that pays for seeks (spinning disk), `Some(false)`
/// for one that does not (SSD/NVMe), `None` when the device will not say.
///
/// `None` means "assume a seek penalty": the conservative single-shard choice
/// is never wrong, only occasionally slower.
pub fn has_seek_penalty(path: &Path) -> io::Result<Option<bool>> {
    platform::has_seek_penalty(path)
}

#[cfg(windows)]
mod platform {
    use super::{DiskSpace, Path, PathBuf, VolumeCaps, io};
    use crate::win::wide;
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::fs::OpenOptionsExt;

    use windows::Win32::Foundation::MAX_PATH;
    use windows::Win32::Storage::FileSystem::{
        FILE_SHARE_READ, FILE_SHARE_WRITE, GetDiskFreeSpaceExW, GetDriveTypeW,
        GetVolumeInformationW, GetVolumePathNameW,
    };
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::{
        DEVICE_SEEK_PENALTY_DESCRIPTOR, IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery,
        STORAGE_PROPERTY_QUERY, StorageDeviceSeekPenaltyProperty,
    };
    use windows::Win32::System::SystemServices::{FILE_SUPPORTS_SPARSE_FILES, FILE_VOLUME_QUOTAS};
    use windows::Win32::System::WindowsProgramming::DRIVE_FIXED;
    use windows::core::PCWSTR;

    fn wide_to_string(buffer: &[u16]) -> String {
        let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        std::ffi::OsString::from_wide(&buffer[..end]).to_string_lossy().into_owned()
    }

    /// Volume root for a path, e.g. `D:\` — the form the volume APIs require.
    fn volume_root(path: &Path) -> io::Result<PathBuf> {
        let wide_path = wide(path);
        let mut buffer = [0u16; MAX_PATH as usize + 1];
        // SAFETY: `wide_path` is NUL-terminated and outlives the call, and
        // `buffer` is a writable slice whose length the callee is given.
        unsafe { GetVolumePathNameW(PCWSTR(wide_path.as_ptr()), &mut buffer) }
            .map_err(|error| io::Error::other(format!("GetVolumePathNameW: {error}")))?;
        Ok(PathBuf::from(wide_to_string(&buffer)))
    }

    pub(super) fn caps_for(path: &Path) -> io::Result<VolumeCaps> {
        let root = volume_root(path)?;
        let wide_root = wide(&root);

        let mut flags: u32 = 0;
        let mut filesystem = [0u16; 32];
        // SAFETY: `wide_root` is NUL-terminated; the out-pointers are valid for
        // the duration of the call and sized as the API expects.
        unsafe {
            GetVolumeInformationW(
                PCWSTR(wide_root.as_ptr()),
                None,
                None,
                None,
                Some(&raw mut flags),
                Some(&mut filesystem),
            )
        }
        .map_err(|error| io::Error::other(format!("GetVolumeInformationW: {error}")))?;

        // SAFETY: `wide_root` is a NUL-terminated wide string.
        let drive_type = unsafe { GetDriveTypeW(PCWSTR(wide_root.as_ptr())) };

        let space = disk_space(&root)?;

        Ok(VolumeCaps {
            filesystem: wide_to_string(&filesystem),
            supports_sparse: flags & FILE_SUPPORTS_SPARSE_FILES != 0,
            supports_quotas: flags & FILE_VOLUME_QUOTAS != 0,
            quota_restricted: space.quota_restricted(),
            is_fixed: drive_type == DRIVE_FIXED,
            root,
        })
    }

    pub(super) fn disk_space(path: &Path) -> io::Result<DiskSpace> {
        let root = volume_root(path)?;
        let wide_root = wide(&root);
        let mut available: u64 = 0;
        let mut total: u64 = 0;
        let mut total_free: u64 = 0;
        // SAFETY: NUL-terminated input; all three out-pointers are valid for
        // the duration of the call.
        unsafe {
            GetDiskFreeSpaceExW(
                PCWSTR(wide_root.as_ptr()),
                Some(&raw mut available),
                Some(&raw mut total),
                Some(&raw mut total_free),
            )
        }
        .map_err(|error| io::Error::other(format!("GetDiskFreeSpaceExW: {error}")))?;
        Ok(DiskSpace { available_to_caller: available, total_free, total })
    }

    pub(super) fn has_seek_penalty(path: &Path) -> io::Result<Option<bool>> {
        let root = volume_root(path)?;
        // The query goes to the volume device, addressed without a trailing
        // separator: `\\.\D:`.
        let letter = root.to_string_lossy().trim_end_matches(['\\', '/']).to_string();
        let device = format!(r"\\.\{letter}");

        // Access mode 0 is enough for an informational IOCTL and, unlike read
        // access, does not require elevation.
        let Ok(volume) = std::fs::OpenOptions::new()
            .access_mode(0)
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0)
            .open(&device)
        else {
            // Not being able to ask is not an error; None means "assume a
            // seek penalty".
            return Ok(None);
        };

        let query = STORAGE_PROPERTY_QUERY {
            PropertyId: StorageDeviceSeekPenaltyProperty,
            QueryType: PropertyStandardQuery,
            AdditionalParameters: [0],
        };
        let mut descriptor = DEVICE_SEEK_PENALTY_DESCRIPTOR::default();
        let mut returned: u32 = 0;

        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        // SAFETY: `volume` is a live handle outliving the call. Both buffers are
        // correctly sized and aligned for their declared types, and the lengths
        // passed match them exactly, so the kernel cannot write out of bounds.
        let result = unsafe {
            DeviceIoControl(
                HANDLE(volume.as_raw_handle()),
                IOCTL_STORAGE_QUERY_PROPERTY,
                Some((&raw const query).cast()),
                size_of::<STORAGE_PROPERTY_QUERY>() as u32,
                Some((&raw mut descriptor).cast()),
                size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() as u32,
                Some(&raw mut returned),
                None,
            )
        };

        match result {
            Ok(()) if returned as usize >= size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() => {
                Ok(Some(descriptor.IncursSeekPenalty))
            }
            // A device that declines to answer is reported as unknown.
            _ => Ok(None),
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use super::{DiskSpace, Path, PathBuf, VolumeCaps, io};

    pub(super) fn caps_for(_path: &Path) -> io::Result<VolumeCaps> {
        Ok(VolumeCaps {
            root: PathBuf::from("/"),
            filesystem: "unknown".into(),
            supports_sparse: false,
            supports_quotas: false,
            quota_restricted: false,
            is_fixed: true,
        })
    }

    pub(super) fn disk_space(_path: &Path) -> io::Result<DiskSpace> {
        Ok(DiskSpace { available_to_caller: u64::MAX, total_free: u64::MAX, total: u64::MAX })
    }

    pub(super) fn has_seek_penalty(_path: &Path) -> io::Result<Option<bool>> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_coherent_caps_for_the_temp_volume() {
        let dir = tempfile::tempdir().unwrap();
        let caps = caps_for(dir.path()).unwrap();

        assert!(!caps.filesystem.is_empty(), "filesystem name should be known");
        assert!(caps.root.as_os_str().len() >= 3, "volume root looks wrong: {:?}", caps.root);
        if caps.filesystem == "NTFS" {
            assert!(caps.supports_sparse, "NTFS must report sparse support");
        }
        assert!(!(caps.should_try_sparse() && !caps.supports_sparse));
    }

    #[test]
    fn quota_support_alone_does_not_block_the_sparse_path() {
        // The regression this guards: NTFS always sets FILE_VOLUME_QUOTAS, so
        // treating that flag as "a quota applies" disabled sparse preallocation
        // on every ordinary Windows volume -- including both of this machine's.
        let dir = tempfile::tempdir().unwrap();
        let caps = caps_for(dir.path()).unwrap();
        if caps.filesystem == "NTFS" && !caps.quota_restricted {
            assert!(
                caps.should_try_sparse(),
                "an unquotaed NTFS volume must take the sparse path (supports_quotas={})",
                caps.supports_quotas
            );
        }
    }

    #[test]
    fn disk_space_figures_are_internally_consistent() {
        let dir = tempfile::tempdir().unwrap();
        let space = disk_space(dir.path()).unwrap();
        assert!(space.total > 0, "volume reported zero total bytes");
        assert!(space.total_free <= space.total);
        assert!(
            space.available_to_caller <= space.total_free,
            "caller-available ({}) exceeded total free ({})",
            space.available_to_caller,
            space.total_free
        );
        // `free_space` returns the caller-available figure, but the volume is
        // live -- sibling tests here allocate 64 MiB files concurrently -- so
        // two reads a moment apart legitimately differ. Assert the magnitude
        // agrees rather than demanding equality, which would be flaky.
        let second_read = free_space(dir.path()).unwrap();
        let drift = space.available_to_caller.abs_diff(second_read);
        assert!(
            drift < 4 << 30,
            "free space moved {drift} bytes between two reads, which is not plausible drift"
        );
    }

    #[test]
    fn seek_penalty_query_never_errors() {
        // Advisory: what matters is that an unavailable answer comes back as
        // None rather than failing the download.
        let dir = tempfile::tempdir().unwrap();
        let answer = has_seek_penalty(dir.path()).unwrap();
        println!("seek penalty: {answer:?}");
    }
}
