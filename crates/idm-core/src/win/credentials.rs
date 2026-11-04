//! Keeping passwords in Windows' own vault.
//!
//! A download manager that signs in to sites and proxies has to keep
//! passwords somewhere. The options were DPAPI directly or Credential
//! Manager, and this is Credential Manager — which is DPAPI underneath
//! anyway, so the choice is not about the cryptography. It is about who
//! owns the result:
//!
//! * DPAPI directly means inventing a file format, owning it, and owning a
//!   key-rotation story for it. Credential Manager is the documented place
//!   for exactly this, and brings all of that already written.
//! * It is **visible and revocable in Control Panel**. A password this app
//!   holds can be deleted without running this app, by somebody who no
//!   longer trusts it or no longer has it installed. A blob in our own
//!   AppData folder has no such story, and "uninstall and hope" is not one.
//!
//! Two rules the rest of the program leans on, enforced here:
//!
//! * **A secret never crosses a boundary it does not have to.** [`Entry`]
//!   is what listing returns and carries no password at all; only
//!   [`Vault::read`], called at the moment a request is being built, yields
//!   one. [`Secret`] prints as `***` however it is formatted, so a stray
//!   `{:?}` in a log or an error cannot leak it.
//! * **Only this app's own entries are ever looked at.** Listing takes a
//!   prefix and the enumeration is filtered by it in the Win32 call, so a
//!   bug here cannot turn into reading the user's bank or VPN passwords.
//!   There is no way to ask this module for everything.
//!
//! Naming the targets is deliberately not done here: it is policy, it
//! belongs to the engine, and core has no business knowing what an HTTP
//! realm is.

use std::fmt;

#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error("{0}")]
    Refused(String),
    #[error("the credential store said: {0}")]
    System(String),
    #[error("this build cannot reach the credential store")]
    Unsupported,
}

type Result<T> = std::result::Result<T, VaultError>;

/// A password, which does not print itself.
///
/// Both `Debug` and `Display` are deliberate: `{}` in an error message and
/// `{:?}` in a tracing field are the two ways a secret escapes, and a type
/// that is careful about one and not the other is not careful.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The password itself, at the point it is about to be used.
    ///
    /// Named so that reading it is conspicuous in a diff. Every call should
    /// be building a header or a proxy URL and nothing else.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

/// What is stored under one target, without the password.
///
/// The shape everything above this module sees, so that showing the user
/// what is saved never involves the secret being anywhere near the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub target: String,
    pub username: String,
}

/// What is stored under one target, with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credential {
    pub username: String,
    pub secret: Secret,
}

/// Windows caps a credential blob at 5 × `CRED_MAX_VALUE_SIZE`.
///
/// Checked here rather than left to `CredWriteW`, so somebody pasting
/// something absurd into the password box is told why instead of getting a
/// Win32 error code.
pub const MAX_SECRET_BYTES: usize = 5 * 512;

/// `CRED_MAX_GENERIC_TARGET_NAME_LENGTH`.
pub const MAX_TARGET_CHARS: usize = 32767;

/// Where passwords are kept.
///
/// A trait so the engine's policy — target naming, host canonicalisation,
/// what is refused — is testable without writing to the machine's real
/// vault and without leaving test entries behind on a developer's profile.
pub trait Vault: Send + Sync {
    /// Stores, replacing anything already under this target.
    fn write(&self, target: &str, username: &str, secret: &Secret) -> Result<()>;

    /// Reads one, or `None` when nothing is stored under that target.
    ///
    /// Absent is not an error: asking whether there is a password for a
    /// host is the normal first step of trying to use one.
    fn read(&self, target: &str) -> Result<Option<Credential>>;

    /// Removes one. Removing what is not there succeeds.
    ///
    /// Idempotent on purpose: the user's intent is "there should be no
    /// password for this", and that is already true.
    fn remove(&self, target: &str) -> Result<()>;

    /// Every entry whose target begins with `prefix`, without passwords.
    ///
    /// The prefix is not a convenience. It is the only way to enumerate,
    /// so this module cannot be asked for credentials that are not ours.
    fn list(&self, prefix: &str) -> Result<Vec<Entry>>;
}

/// Checks that apply whatever the backend is.
///
/// Here rather than in each implementation so the fake used in tests
/// refuses exactly what the real vault refuses — otherwise the tests prove
/// something about the fake.
fn vet(target: &str, secret: Option<&Secret>) -> Result<()> {
    if target.trim().is_empty() {
        return Err(VaultError::Refused("a credential needs a name to be stored under".into()));
    }
    if target.chars().count() > MAX_TARGET_CHARS {
        return Err(VaultError::Refused("that credential name is too long to store".into()));
    }
    // A target with an embedded NUL would be truncated by the wide-string
    // conversion, so what was written and what was asked for would differ.
    if target.contains('\0') {
        return Err(VaultError::Refused("a credential name cannot contain a null".into()));
    }
    if let Some(secret) = secret {
        // UTF-16 is what goes into the blob, so the limit is counted there
        // rather than in bytes of UTF-8.
        let bytes = secret.expose().encode_utf16().count() * 2;
        if bytes > MAX_SECRET_BYTES {
            return Err(VaultError::Refused(
                "that password is longer than Windows will store".into(),
            ));
        }
    }
    Ok(())
}

/// The machine's own Credential Manager.
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsVault;

impl Vault for WindowsVault {
    fn write(&self, target: &str, username: &str, secret: &Secret) -> Result<()> {
        vet(target, Some(secret))?;
        platform::write(target, username, secret.expose())
    }

    fn read(&self, target: &str) -> Result<Option<Credential>> {
        vet(target, None)?;
        platform::read(target)
    }

    fn remove(&self, target: &str) -> Result<()> {
        vet(target, None)?;
        platform::remove(target)
    }

    fn list(&self, prefix: &str) -> Result<Vec<Entry>> {
        if prefix.trim().is_empty() {
            // The guard that makes the prefix a boundary rather than a
            // filter. Without it this would return every credential on the
            // machine, which is not something this program should ever
            // hold, even in a local variable.
            return Err(VaultError::Refused("listing every credential is not allowed".into()));
        }
        vet(prefix, None)?;
        platform::list(prefix)
    }
}

#[cfg(windows)]
mod platform {
    use super::{Credential, Entry, Secret, VaultError};

    use windows::Win32::Foundation::{ERROR_NOT_FOUND, GetLastError};
    use windows::Win32::Security::Credentials::{
        CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredEnumerateW,
        CredFree, CredReadW, CredWriteW,
    };
    use windows::core::PWSTR;

    type Result<T> = std::result::Result<T, VaultError>;

    /// A NUL-terminated UTF-16 buffer that outlives the call using it.
    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Reads a NUL-terminated wide string Windows allocated.
    ///
    /// # Safety
    ///
    /// `pointer` must be NUL-terminated or null.
    unsafe fn from_wide(pointer: PWSTR) -> String {
        if pointer.is_null() {
            return String::new();
        }
        // SAFETY: the caller guarantees termination; `to_string` walks to
        // the NUL and copies, so nothing borrows the Windows allocation
        // after it is freed.
        unsafe { pointer.to_string().unwrap_or_default() }
    }

    fn last_error(what: &str) -> VaultError {
        // SAFETY: no preconditions; reads this thread's last error.
        let code = unsafe { GetLastError() };
        VaultError::System(format!("{what} failed with {}", code.0))
    }

    pub(super) fn write(target: &str, username: &str, secret: &str) -> Result<()> {
        let mut target_wide = wide(target);
        let mut user_wide = wide(username);
        // UTF-16LE with no terminator, which is what every other program
        // that reads a generic credential blob expects a password to be.
        let mut blob: Vec<u8> = secret.encode_utf16().flat_map(|unit| unit.to_le_bytes()).collect();

        let credential = CREDENTIALW {
            Type: CRED_TYPE_GENERIC,
            TargetName: PWSTR(target_wide.as_mut_ptr()),
            CredentialBlobSize: blob.len() as u32,
            CredentialBlob: blob.as_mut_ptr(),
            // LOCAL_MACHINE rather than ENTERPRISE, deliberately: ENTERPRISE
            // roams the credential to a domain profile, which sends this
            // user's password for a download site to their employer's
            // servers. Nobody asked for that by saving a password here.
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            UserName: PWSTR(user_wide.as_mut_ptr()),
            ..Default::default()
        };

        // SAFETY: every pointer in `credential` is into a live local that
        // outlives this call, and the sizes match the buffers they describe.
        unsafe { CredWriteW(&credential, 0) }.map_err(|_| last_error("storing the password"))
    }

    pub(super) fn read(target: &str) -> Result<Option<Credential>> {
        let target_wide = wide(target);
        let mut pointer = std::ptr::null_mut();

        // SAFETY: `target_wide` is NUL-terminated and `pointer` is a valid
        // out-parameter; on success it owns an allocation freed below.
        let outcome = unsafe {
            CredReadW(
                windows::core::PCWSTR(target_wide.as_ptr()),
                CRED_TYPE_GENERIC,
                None,
                &mut pointer,
            )
        };

        if outcome.is_err() {
            // SAFETY: no preconditions.
            if unsafe { GetLastError() } == ERROR_NOT_FOUND {
                // Nothing stored, which is an answer rather than a failure.
                return Ok(None);
            }
            return Err(last_error("reading the password"));
        }

        // SAFETY: CredReadW succeeded, so `pointer` is a live CREDENTIALW.
        let credential = unsafe { &*pointer };
        // SAFETY: Windows terminates the strings it allocates here.
        let username = unsafe { from_wide(credential.UserName) };
        let size = credential.CredentialBlobSize as usize;
        let secret = if credential.CredentialBlob.is_null() || size == 0 {
            String::new()
        } else {
            // SAFETY: the blob is `size` bytes at that pointer, per the
            // struct Windows just filled in.
            let bytes = unsafe { std::slice::from_raw_parts(credential.CredentialBlob, size) };
            decode_blob(bytes)
        };

        // SAFETY: freeing exactly what CredReadW allocated, once, after
        // everything borrowed from it has been copied out.
        unsafe { CredFree(pointer as *const _) };

        Ok(Some(Credential { username, secret: Secret::new(secret) }))
    }

    /// Turns a credential blob back into text.
    ///
    /// UTF-16LE is the convention and what `write` produces, but the vault
    /// is shared with every other program on the machine and nothing makes
    /// a blob be text at all. An odd length cannot be UTF-16, so it is read
    /// as UTF-8 instead rather than silently losing its last byte.
    pub(super) fn decode_blob(bytes: &[u8]) -> String {
        if bytes.len().is_multiple_of(2) {
            let units: Vec<u16> =
                bytes.as_chunks::<2>().0.iter().map(|pair| u16::from_le_bytes(*pair)).collect();
            if let Ok(text) = String::from_utf16(&units) {
                return text;
            }
        }
        String::from_utf8_lossy(bytes).into_owned()
    }

    pub(super) fn remove(target: &str) -> Result<()> {
        let target_wide = wide(target);
        // SAFETY: `target_wide` is NUL-terminated.
        let outcome = unsafe {
            CredDeleteW(windows::core::PCWSTR(target_wide.as_ptr()), CRED_TYPE_GENERIC, None)
        };
        if outcome.is_err() {
            // SAFETY: no preconditions.
            if unsafe { GetLastError() } == ERROR_NOT_FOUND {
                // Already absent, which is the state that was asked for.
                return Ok(());
            }
            return Err(last_error("removing the password"));
        }
        Ok(())
    }

    pub(super) fn list(prefix: &str) -> Result<Vec<Entry>> {
        // `CredEnumerateW`'s filter matches a trailing `*` and nothing else,
        // so the prefix becomes one here rather than at every call site.
        let filter = wide(&format!("{prefix}*"));
        let mut count = 0u32;
        let mut items = std::ptr::null_mut();

        // SAFETY: `filter` is NUL-terminated; `count` and `items` are valid
        // out-parameters, and on success `items` owns an allocation freed
        // below.
        let outcome = unsafe {
            CredEnumerateW(
                windows::core::PCWSTR(filter.as_ptr()),
                // No flags: the default is "match the filter", and the only
                // other option is ALL_CREDENTIALS, which ignores the filter
                // entirely -- the one thing this module must never do.
                None,
                &mut count,
                &mut items,
            )
        };

        if outcome.is_err() {
            // SAFETY: no preconditions.
            if unsafe { GetLastError() } == ERROR_NOT_FOUND {
                // No entries matched, which is an empty list rather than a
                // failure: it is what a fresh installation looks like.
                return Ok(Vec::new());
            }
            return Err(last_error("listing the saved passwords"));
        }

        let mut entries = Vec::new();
        for index in 0..count as usize {
            // SAFETY: Windows filled `count` pointers at `items`, each to a
            // live CREDENTIALW it owns.
            let credential = unsafe { &**items.add(index) };
            // SAFETY: Windows terminates the strings it allocates here.
            let target = unsafe { from_wide(credential.TargetName) };
            // SAFETY: as above.
            let username = unsafe { from_wide(credential.UserName) };
            // The blob is deliberately not touched. Nothing above this
            // module has any use for a password while listing, and the
            // surest way not to leak one is not to copy it out.
            entries.push(Entry { target, username });
        }

        // SAFETY: freeing exactly what CredEnumerateW allocated, once,
        // after everything borrowed from it has been copied out.
        unsafe { CredFree(items as *const _) };

        Ok(entries)
    }
}

#[cfg(not(windows))]
mod platform {
    use super::{Credential, Entry, VaultError};

    type Result<T> = std::result::Result<T, VaultError>;

    pub(super) fn write(_: &str, _: &str, _: &str) -> Result<()> {
        Err(VaultError::Unsupported)
    }

    pub(super) fn read(_: &str) -> Result<Option<Credential>> {
        Err(VaultError::Unsupported)
    }

    pub(super) fn remove(_: &str) -> Result<()> {
        Err(VaultError::Unsupported)
    }

    pub(super) fn list(_: &str) -> Result<Vec<Entry>> {
        Err(VaultError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// A vault in memory, vetting exactly what the real one vets.
    ///
    /// Shares `vet` with `WindowsVault` rather than reimplementing it, so a
    /// test that proves a refusal here proves it there too.
    #[derive(Default)]
    pub struct FakeVault {
        items: Mutex<BTreeMap<String, Credential>>,
    }

    impl Vault for FakeVault {
        fn write(&self, target: &str, username: &str, secret: &Secret) -> Result<()> {
            vet(target, Some(secret))?;
            self.items.lock().unwrap().insert(
                target.to_owned(),
                Credential { username: username.to_owned(), secret: secret.clone() },
            );
            Ok(())
        }

        fn read(&self, target: &str) -> Result<Option<Credential>> {
            vet(target, None)?;
            Ok(self.items.lock().unwrap().get(target).cloned())
        }

        fn remove(&self, target: &str) -> Result<()> {
            vet(target, None)?;
            self.items.lock().unwrap().remove(target);
            Ok(())
        }

        fn list(&self, prefix: &str) -> Result<Vec<Entry>> {
            if prefix.trim().is_empty() {
                return Err(VaultError::Refused("listing every credential is not allowed".into()));
            }
            vet(prefix, None)?;
            Ok(self
                .items
                .lock()
                .unwrap()
                .iter()
                .filter(|(target, _)| target.starts_with(prefix))
                .map(|(target, credential)| Entry {
                    target: target.clone(),
                    username: credential.username.clone(),
                })
                .collect())
        }
    }

    #[test]
    fn a_secret_does_not_print_itself() {
        // The property the whole type exists for. Both forms, because a
        // tracing field uses one and an error message the other.
        let secret = Secret::new("hunter2");
        assert_eq!(format!("{secret:?}"), "***");
        assert_eq!(format!("{secret}"), "***");
        // And it is still there when deliberately asked for.
        assert_eq!(secret.expose(), "hunter2");
    }

    #[test]
    fn a_credential_does_not_print_its_password_either() {
        // Derived Debug on the struct would be useless if it printed the
        // field, and a struct is what gets logged, not a bare secret.
        let credential = Credential { username: "someone".into(), secret: Secret::new("hunter2") };
        let shown = format!("{credential:?}");
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(shown.contains("someone"), "the username is not a secret: {shown}");
    }

    #[test]
    fn listing_everything_is_refused() {
        // The boundary, not a filter. Without this the program could hold
        // every password on the machine in a local variable.
        let vault = FakeVault::default();
        for prefix in ["", "   "] {
            let error = vault.list(prefix).unwrap_err();
            assert!(error.to_string().contains("not allowed"), "{error}");
        }
    }

    #[test]
    fn listing_shows_only_what_the_prefix_covers() {
        let vault = FakeVault::default();
        vault.write("DivinePulse/http/https://a", "me", &Secret::new("x")).unwrap();
        vault.write("DivinePulse/proxy/p:8080", "me", &Secret::new("y")).unwrap();
        // Somebody else's, which must never come back.
        vault.write("Microsoft/OneDrive", "them", &Secret::new("z")).unwrap();

        let listed = vault.list("DivinePulse/").unwrap();
        assert_eq!(listed.len(), 2, "{listed:?}");
        assert!(listed.iter().all(|entry| entry.target.starts_with("DivinePulse/")), "{listed:?}");
    }

    #[test]
    fn listing_never_carries_a_password() {
        // Enforced by the type rather than by discipline: `Entry` has no
        // field for one, so this cannot regress without a signature change.
        let vault = FakeVault::default();
        vault.write("DivinePulse/http/x", "me", &Secret::new("hunter2")).unwrap();
        let shown = format!("{:?}", vault.list("DivinePulse/").unwrap());
        assert!(!shown.contains("hunter2"), "{shown}");
    }

    #[test]
    fn reading_something_that_is_not_there_is_not_a_failure() {
        // Asking whether a host has a saved password is the normal first
        // step of trying to use one, so absence has to be an answer.
        let vault = FakeVault::default();
        assert_eq!(vault.read("DivinePulse/http/nowhere").unwrap(), None);
    }

    #[test]
    fn removing_something_that_is_not_there_succeeds() {
        // The intent is "there should be no password for this", and that is
        // already true. Failing would make the window show an error for
        // having got what it asked for.
        let vault = FakeVault::default();
        vault.remove("DivinePulse/http/nowhere").expect("removing nothing should succeed");
    }

    #[test]
    fn writing_again_replaces_rather_than_duplicates() {
        let vault = FakeVault::default();
        vault.write("DivinePulse/http/x", "old", &Secret::new("one")).unwrap();
        vault.write("DivinePulse/http/x", "new", &Secret::new("two")).unwrap();

        let found = vault.read("DivinePulse/http/x").unwrap().expect("stored");
        assert_eq!(found.username, "new");
        assert_eq!(found.secret.expose(), "two");
        assert_eq!(vault.list("DivinePulse/").unwrap().len(), 1);
    }

    #[test]
    fn a_nameless_credential_is_refused() {
        let vault = FakeVault::default();
        let error = vault.write("  ", "me", &Secret::new("x")).unwrap_err();
        assert!(error.to_string().contains("needs a name"), "{error}");
    }

    #[test]
    fn a_name_with_a_null_in_it_is_refused() {
        // The wide-string conversion would stop at the null, so what was
        // written and what was asked for would be different targets --
        // which is how a password ends up saved somewhere nobody looks.
        let vault = FakeVault::default();
        let error = vault.write("DivinePulse/http/a\0b", "me", &Secret::new("x")).unwrap_err();
        assert!(error.to_string().contains("null"), "{error}");
    }

    #[test]
    fn a_password_longer_than_windows_will_keep_is_refused_in_words() {
        // Said here rather than let through to come back as a Win32 error
        // code, which is not something to put in front of somebody who has
        // just pasted a password.
        let vault = FakeVault::default();
        let huge = Secret::new("a".repeat(MAX_SECRET_BYTES));
        let error = vault.write("DivinePulse/http/x", "me", &huge).unwrap_err();
        assert!(error.to_string().contains("longer than Windows"), "{error}");

        // And the largest one that does fit is accepted, so the boundary is
        // where it is claimed to be rather than one short.
        let largest = Secret::new("a".repeat(MAX_SECRET_BYTES / 2));
        vault.write("DivinePulse/http/x", "me", &largest).expect("the limit itself should fit");
    }

    #[cfg(windows)]
    #[test]
    fn a_blob_that_is_not_utf16_is_read_rather_than_mangled() {
        // The vault is shared with every other program on the machine, and
        // nothing obliges a generic credential's blob to be UTF-16 -- or to
        // be text at all. An odd length cannot be UTF-16, and reading it as
        // such would silently drop its last byte.
        assert_eq!(super::platform::decode_blob(&[0x61, 0x00, 0x62, 0x00]), "ab");
        assert_eq!(super::platform::decode_blob(b"abc"), "abc");
        assert_eq!(super::platform::decode_blob(&[]), "");
    }
}
