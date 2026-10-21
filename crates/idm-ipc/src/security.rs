//! Who is allowed to talk to the engine.
//!
//! The IPC endpoint can queue downloads, read download history, and see the
//! cookies forwarded with a job. Any local process reaching it would inherit
//! all of that, so access is restricted three ways, deliberately overlapping:
//!
//! 1. **A DACL on the pipe** limiting it to this user's SID, which is the only
//!    one of the three the operating system enforces for us.
//! 2. **An unguessable pipe name**, so a process that somehow has access still
//!    has to find the endpoint.
//! 3. **A per-session token**, held in a file under the user's own profile,
//!    which a client must present as its first message.
//!
//! The DACL is the real control; the other two are depth. A default-security
//! named pipe is readable far more widely than this needs to be, so relying on
//! the name alone would be security by obscurity.

use std::path::{Path, PathBuf};

/// Hex characters in a session token. 32 bytes of entropy.
const TOKEN_BYTES: usize = 32;

#[derive(Debug, thiserror::Error)]
pub enum SecurityError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("could not determine this user's identity: {0}")]
    Identity(String),
    #[error("could not build the pipe's security descriptor: {0}")]
    Descriptor(String),
    #[error("no endpoint is running, or its token file is missing")]
    NoEndpoint,
    #[error("the client presented the wrong token")]
    BadToken,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A shared secret proving a client can read a file only this user can read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionToken(String);

impl SessionToken {
    pub fn generate() -> Self {
        use rand::RngCore;
        let mut bytes = [0u8; TOKEN_BYTES];
        rand::rng().fill_bytes(&mut bytes);
        Self(hex(&bytes))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Compares in constant time.
    ///
    /// A byte-at-a-time comparison leaks how much of the token was right
    /// through timing, which over many attempts is enough to recover it. The
    /// cost of not leaking it is a loop that cannot exit early.
    pub fn matches(&self, presented: &str) -> bool {
        let expected = self.0.as_bytes();
        let actual = presented.as_bytes();
        if expected.len() != actual.len() {
            return false;
        }
        let mut difference = 0u8;
        for (a, b) in expected.iter().zip(actual) {
            difference |= a ^ b;
        }
        difference == 0
    }
}

/// Where the endpoint advertises itself, under the user's own profile.
#[derive(Debug, Clone, Serialize_, Deserialize_)]
pub struct Endpoint {
    pub pipe_name: String,
    pub token: String,
    pub pid: u32,
}

// Local aliases so the derive macros stay readable above.
use serde::Deserialize as Deserialize_;
use serde::Serialize as Serialize_;

/// `%LOCALAPPDATA%\DivineIDM\endpoint.json`, or the platform equivalent.
pub fn endpoint_path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("DivineIDM").join("endpoint.json")
}

/// An unguessable pipe name, so the endpoint cannot simply be found by name.
pub fn random_pipe_name() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    format!(r"\\.\pipe\divine-idm-{}", hex(&bytes))
}

impl Endpoint {
    pub fn new(pipe_name: String, token: &SessionToken) -> Self {
        Self { pipe_name, token: token.as_str().to_owned(), pid: std::process::id() }
    }

    /// Publishes the endpoint for clients on this machine.
    pub fn write(&self, path: &Path) -> Result<(), SecurityError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    pub fn read(path: &Path) -> Result<Self, SecurityError> {
        let bytes = std::fs::read(path).map_err(|_| SecurityError::NoEndpoint)?;
        serde_json::from_slice(&bytes).map_err(|_| SecurityError::NoEndpoint)
    }

    pub fn remove(path: &Path) {
        let _ = std::fs::remove_file(path);
    }
}

/// An SDDL string granting full control to this user alone.
///
/// `D:P` makes the DACL protected, so it does not inherit anything more
/// permissive from elsewhere; the single ACE grants generic-all to this user's
/// SID and nothing is granted to anyone else.
pub fn user_only_sddl() -> Result<String, SecurityError> {
    let sid = platform::current_user_sid()?;
    Ok(format!("D:P(A;;GA;;;{sid})"))
}

#[cfg(windows)]
pub mod platform {
    use super::SecurityError;

    use windows::Win32::Foundation::{HANDLE, LocalFree};
    use windows::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows::Win32::Security::{
        GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
        TokenUser,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::core::PWSTR;

    fn wide_to_string(ptr: PWSTR) -> String {
        if ptr.is_null() {
            return String::new();
        }
        // SAFETY: the pointer came from a Win32 call that returns a
        // NUL-terminated wide string and has not yet been freed.
        unsafe { ptr.to_string().unwrap_or_default() }
    }

    /// This process's user SID in string form, e.g. `S-1-5-21-...`.
    pub fn current_user_sid() -> Result<String, SecurityError> {
        let mut token = HANDLE::default();
        // SAFETY: GetCurrentProcess returns a pseudo-handle needing no close;
        // `token` is a valid out-pointer.
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) }
            .map_err(|error| SecurityError::Identity(format!("OpenProcessToken: {error}")))?;

        let mut needed = 0u32;
        // First call sizes the buffer; it is expected to fail with
        // ERROR_INSUFFICIENT_BUFFER, so the error is deliberately ignored.
        // SAFETY: passing a null buffer with length 0 is the documented way to
        // ask for the required size.
        let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &raw mut needed) };

        let mut buffer = vec![0u8; needed as usize];
        // SAFETY: `buffer` is at least `needed` bytes, which is what the call
        // above reported it requires.
        let result = unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                Some(buffer.as_mut_ptr().cast()),
                needed,
                &raw mut needed,
            )
        };
        // SAFETY: `token` came from OpenProcessToken and is not used again.
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(token) };
        result.map_err(|error| SecurityError::Identity(format!("GetTokenInformation: {error}")))?;

        // SAFETY: on success the buffer holds a TOKEN_USER whose Sid points
        // into that same buffer, which outlives this borrow.
        let user: &TOKEN_USER = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
        let mut sid_text = PWSTR::null();
        // SAFETY: the SID is valid for the lifetime of `buffer`.
        unsafe { ConvertSidToStringSidW(user.User.Sid, &raw mut sid_text) }
            .map_err(|error| SecurityError::Identity(format!("ConvertSidToStringSidW: {error}")))?;

        let text = wide_to_string(sid_text);
        // SAFETY: ConvertSidToStringSidW allocates with LocalAlloc.
        unsafe {
            let _ = LocalFree(Some(windows::Win32::Foundation::HLOCAL(sid_text.0.cast())));
        }
        if text.is_empty() {
            return Err(SecurityError::Identity("empty SID".into()));
        }
        Ok(text)
    }

    /// Owns a security descriptor and the `SECURITY_ATTRIBUTES` pointing at it.
    ///
    /// Kept together because the attributes borrow the descriptor: letting the
    /// descriptor drop first would leave a dangling pointer for the kernel to
    /// read.
    pub struct PipeSecurity {
        descriptor: PSECURITY_DESCRIPTOR,
        attributes: SECURITY_ATTRIBUTES,
    }

    impl PipeSecurity {
        /// Builds attributes granting access to this user alone.
        pub fn user_only() -> Result<Self, SecurityError> {
            let sddl = super::user_only_sddl()?;
            let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
            let mut descriptor = PSECURITY_DESCRIPTOR::default();

            // SAFETY: `wide` is a NUL-terminated UTF-16 SDDL string that
            // outlives the call; `descriptor` is a valid out-pointer.
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    windows::core::PCWSTR(wide.as_ptr()),
                    SDDL_REVISION_1,
                    &raw mut descriptor,
                    None,
                )
            }
            .map_err(|error| SecurityError::Descriptor(format!("{error} (sddl: {sddl})")))?;

            let attributes = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor.0,
                bInheritHandle: false.into(),
            };
            Ok(Self { descriptor, attributes })
        }

        /// A pointer for `ServerOptions::create_with_security_attributes_raw`.
        pub fn as_raw(&self) -> *mut std::ffi::c_void {
            (&raw const self.attributes).cast_mut().cast()
        }
    }

    // SAFETY: `descriptor` is a LocalAlloc'd block owned solely by this value,
    // and `attributes` points only into that block. Nothing else aliases
    // either, and neither is mutated after construction, so moving the pair to
    // another thread is sound. This is needed because the serve loop holds the
    // security across an await and is spawned onto the runtime.
    unsafe impl Send for PipeSecurity {}

    impl Drop for PipeSecurity {
        fn drop(&mut self) {
            if !self.descriptor.0.is_null() {
                // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW,
                // which documents LocalFree as the way to release it.
                unsafe {
                    let _ = LocalFree(Some(windows::Win32::Foundation::HLOCAL(self.descriptor.0)));
                }
            }
        }
    }
}

#[cfg(not(windows))]
pub mod platform {
    use super::SecurityError;

    pub fn current_user_sid() -> Result<String, SecurityError> {
        Err(SecurityError::Identity("SIDs are Windows-only".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_long_and_unique() {
        let first = SessionToken::generate();
        let second = SessionToken::generate();
        assert_eq!(first.as_str().len(), TOKEN_BYTES * 2);
        assert_ne!(first, second, "two tokens collided, which should be impossible");
        assert!(first.as_str().chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn token_comparison_accepts_only_the_exact_token() {
        let token = SessionToken::generate();
        assert!(token.matches(token.as_str()));
        assert!(!token.matches(""));
        assert!(!token.matches("short"));
        // A near miss must fail just as surely as a wild guess.
        let mut almost = token.as_str().to_owned();
        almost.pop();
        almost.push(if token.as_str().ends_with('a') { 'b' } else { 'a' });
        assert!(!token.matches(&almost));
        // And a correct prefix must not help.
        assert!(!token.matches(&token.as_str()[..16]));
    }

    #[test]
    fn pipe_names_are_unguessable_and_distinct() {
        let first = random_pipe_name();
        let second = random_pipe_name();
        assert_ne!(first, second);
        assert!(first.starts_with(r"\\.\pipe\divine-idm-"));
        // 16 bytes of entropy, hex encoded.
        assert_eq!(first.len(), r"\\.\pipe\divine-idm-".len() + 32);
    }

    #[test]
    fn an_endpoint_round_trips_through_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("endpoint.json");
        let token = SessionToken::generate();
        let endpoint = Endpoint::new(random_pipe_name(), &token);

        endpoint.write(&path).unwrap();
        let read = Endpoint::read(&path).unwrap();
        assert_eq!(read.pipe_name, endpoint.pipe_name);
        assert!(token.matches(&read.token));
        assert_eq!(read.pid, std::process::id());

        Endpoint::remove(&path);
        assert!(matches!(Endpoint::read(&path), Err(SecurityError::NoEndpoint)));
    }

    #[cfg(windows)]
    #[test]
    fn the_pipe_dacl_names_this_user_and_nobody_else() {
        let sid = platform::current_user_sid().expect("should know who we are");
        assert!(sid.starts_with("S-1-"), "unexpected SID form: {sid}");

        let sddl = user_only_sddl().unwrap();
        // Protected, so nothing more permissive is inherited, and exactly one
        // ACE, for this user.
        assert!(sddl.starts_with("D:P("), "DACL should be protected: {sddl}");
        assert!(sddl.contains(&sid), "DACL should name this user: {sddl}");
        assert_eq!(sddl.matches('(').count(), 1, "exactly one ACE expected: {sddl}");

        // None of the well-known groups that a default pipe would admit.
        for well_known in ["WD", "AU", "BU", "AN", "S-1-1-0"] {
            assert!(
                !sddl.contains(&format!(";{well_known})")),
                "{well_known} should not be granted access: {sddl}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn security_attributes_build_from_the_sddl() {
        let security = platform::PipeSecurity::user_only().expect("should build");
        assert!(!security.as_raw().is_null());
    }
}
