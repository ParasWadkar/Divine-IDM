//! Acquiring `SeManageVolumePrivilege`, which `SetFileValidData` requires.
//!
//! That privilege is held by Administrators and almost nobody else. Its absence
//! is entirely normal and is not an error: the engine simply takes the sparse
//! path instead, which reaches the same throughput without needing it.
//!
//! Worth being clear about *why* the privilege is gated at all.
//! `SetFileValidData` tells NTFS to treat the whole file as initialised without
//! writing anything, so the bytes a reader sees in not-yet-written regions are
//! whatever was previously on that disk — potentially another user's deleted
//! data. That is why it is an opt-in here and never the default.

use std::io;
use std::sync::OnceLock;

/// Whether this process holds `SeManageVolumePrivilege`, enabling it on first
/// call if the account is allowed to.
///
/// Cached: the answer cannot change for the lifetime of the process, and the
/// cost is a few syscalls.
pub fn have_manage_volume_privilege() -> bool {
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| match platform::enable_manage_volume() {
        Ok(enabled) => enabled,
        Err(error) => {
            tracing::debug!(%error, "could not enable SeManageVolumePrivilege");
            false
        }
    })
}

#[cfg(windows)]
mod platform {
    use super::io;

    use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, GetLastError, HANDLE, LUID};
    use windows::Win32::Security::{
        AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_MANAGE_VOLUME_NAME,
        SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::core::PCWSTR;

    pub(super) fn enable_manage_volume() -> io::Result<bool> {
        let mut token = HANDLE::default();
        // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no
        // closing; `token` is a valid out-pointer for the duration of the call.
        unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                &raw mut token,
            )
        }
        .map_err(|error| io::Error::other(format!("OpenProcessToken: {error}")))?;

        // Close the token however this function exits.
        let result = enable_on_token(token);
        // SAFETY: `token` was produced by OpenProcessToken above and is not
        // used again after this point.
        let _ = unsafe { CloseHandle(token) };
        result
    }

    fn enable_on_token(token: HANDLE) -> io::Result<bool> {
        let mut luid = LUID::default();
        // SAFETY: SE_MANAGE_VOLUME_NAME is a static NUL-terminated wide string
        // from the bindings; `luid` is a valid out-pointer.
        unsafe { LookupPrivilegeValueW(PCWSTR::null(), SE_MANAGE_VOLUME_NAME, &raw mut luid) }
            .map_err(|error| io::Error::other(format!("LookupPrivilegeValueW: {error}")))?;

        let privileges = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
        };

        // SAFETY: `token` is live, and `privileges` is a correctly initialised
        // TOKEN_PRIVILEGES whose declared PrivilegeCount matches its array.
        unsafe { AdjustTokenPrivileges(token, false, Some(&raw const privileges), 0, None, None) }
            .map_err(|error| io::Error::other(format!("AdjustTokenPrivileges: {error}")))?;

        // The call reports success even when the privilege is not held by the
        // account; only GetLastError distinguishes the two. Checking just the
        // return value would make us believe we had a privilege we do not,
        // and SetFileValidData would then fail mid-download instead of at
        // setup, after the sparse decision had already been made.
        // SAFETY: reads the calling thread's last-error value.
        let last_error = unsafe { GetLastError() };
        Ok(last_error == ERROR_SUCCESS)
    }
}

#[cfg(not(windows))]
mod platform {
    use super::io;

    pub(super) fn enable_manage_volume() -> io::Result<bool> {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privilege_query_is_decisive_and_stable() {
        // Either answer is legitimate; what matters is that it never panics and
        // never changes within a process, since the write mode is chosen once
        // from it.
        let first = have_manage_volume_privilege();
        let second = have_manage_volume_privilege();
        assert_eq!(first, second, "privilege answer must be stable");
        println!("SeManageVolumePrivilege held: {first}");
    }
}
