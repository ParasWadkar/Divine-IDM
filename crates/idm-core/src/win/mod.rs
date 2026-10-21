//! Windows-specific file primitives the engine depends on.
//!
//! Kept together so the `unsafe` surface is small, auditable, and covered by
//! tests, rather than scattered through the writer and the journal.

mod identity;
pub mod power;
pub mod prealloc;
pub mod privilege;
pub mod volume;

pub use identity::FileIdentity;
pub use prealloc::{PreallocOptions, Preallocation, WriteMode};
pub use volume::VolumeCaps;

/// A NUL-terminated UTF-16 buffer, as every wide Win32 entry point wants.
#[cfg(windows)]
pub(crate) fn wide(path: &std::path::Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}
