//! The real Credential Manager, not the fake.
//!
//! The unit tests prove the rules; this proves the Win32 calls underneath
//! them — the wide-string conversions, the blob encoding, the ownership of
//! what Windows allocates, and that `ERROR_NOT_FOUND` is read as an answer
//! rather than a failure. None of that is exercised by an in-memory map.
//!
//! Everything is written under a prefix of its own and removed again,
//! including when an assertion fails partway: a test that leaves a
//! credential behind on a developer's profile has made a mess of something
//! visible in Control Panel.

#![cfg(windows)]

use idm_core::win::credentials::{Secret, Vault, WindowsVault};

/// Distinct from anything the engine uses, so a failure here cannot disturb
/// a real saved password and a stray entry is obvious for what it is.
const PREFIX: &str = "DivinePulse/test/";

/// One test's own corner of the vault, cleared when the test ends.
///
/// A corner each, not a shared one: these run in parallel by default, so a
/// shared prefix means one test's cleanup deleting another's entries and
/// one test's listing counting them. That is not a hypothetical -- it is
/// what the first run of this file did.
///
/// A guard rather than a call at the end of each test, because the end of a
/// test is exactly what a failed assertion skips.
struct Scope(String);

impl Scope {
    fn new(name: &str) -> Self {
        let scope = Self(format!("{PREFIX}{name}/"));
        // Anything a previous interrupted run left behind, so a test does
        // not inherit it and fail for somebody else's reason.
        scope.clear();
        scope
    }

    fn prefix(&self) -> &str {
        &self.0
    }

    fn target(&self, name: &str) -> String {
        format!("{}{name}", self.0)
    }

    fn clear(&self) {
        let vault = WindowsVault;
        if let Ok(entries) = vault.list(&self.0) {
            for entry in entries {
                let _ = vault.remove(&entry.target);
            }
        }
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        self.clear();
    }
}

#[test]
fn a_password_survives_a_round_trip_through_the_vault() {
    let scope = Scope::new("roundtrip");
    let vault = WindowsVault;
    let target = scope.target("roundtrip");

    vault
        .write(&target, "someone@example.invalid", &Secret::new("hunter2"))
        .expect("writing should succeed");

    let found = vault.read(&target).expect("reading should succeed").expect("it should be there");
    assert_eq!(found.username, "someone@example.invalid");
    assert_eq!(found.secret.expose(), "hunter2");
}

#[test]
fn a_password_with_characters_outside_ascii_comes_back_as_it_went_in() {
    // The blob is UTF-16LE, and the two ways to get this wrong -- counting
    // bytes as units, or losing a surrogate pair -- both survive an ASCII
    // test. An emoji is a surrogate pair; the rest is a spread of scripts.
    let scope = Scope::new("wide");
    let vault = WindowsVault;
    let target = scope.target("wide");
    let password = "pâ€ssword-日本語-Ω-🔑";

    vault.write(&target, "ünïcode", &Secret::new(password)).expect("writing should succeed");

    let found = vault.read(&target).expect("reading").expect("it should be there");
    assert_eq!(found.secret.expose(), password);
    assert_eq!(found.username, "ünïcode");
}

#[test]
fn reading_a_target_that_was_never_written_is_none() {
    // ERROR_NOT_FOUND has to come back as an answer. Treated as a failure
    // it would mean every "is there a password for this host?" produced an
    // error on the overwhelmingly common path.
    let scope = Scope::new("absent");
    let vault = WindowsVault;
    assert_eq!(vault.read(&scope.target("never-written")).expect("reading should succeed"), None);
}

#[test]
fn removing_leaves_nothing_behind_and_removing_again_still_succeeds() {
    let scope = Scope::new("removable");
    let vault = WindowsVault;
    let target = scope.target("removable");

    vault.write(&target, "me", &Secret::new("x")).expect("writing");
    vault.remove(&target).expect("removing");
    assert_eq!(vault.read(&target).expect("reading"), None, "it should be gone");
    // Idempotent: the state asked for is already the state.
    vault.remove(&target).expect("removing what is not there should succeed");
}

#[test]
fn writing_the_same_target_twice_replaces_it() {
    let scope = Scope::new("replaced");
    let vault = WindowsVault;
    let target = scope.target("replaced");

    vault.write(&target, "first", &Secret::new("one")).expect("writing");
    vault.write(&target, "second", &Secret::new("two")).expect("writing again");

    let found = vault.read(&target).expect("reading").expect("it should be there");
    assert_eq!(found.username, "second");
    assert_eq!(found.secret.expose(), "two");
    assert_eq!(
        vault.list(scope.prefix()).expect("listing").len(),
        1,
        "a second write must replace rather than add",
    );
}

#[test]
fn listing_returns_this_prefix_and_nothing_else() {
    // The property that keeps this program away from the user's other
    // saved passwords. The machine running this almost certainly has some
    // -- Windows itself stores them -- so an unfiltered enumeration would
    // show up here as a count far larger than three.
    let scope = Scope::new("listing");
    let vault = WindowsVault;

    for name in ["alpha", "beta", "gamma"] {
        vault.write(&scope.target(name), name, &Secret::new("x")).expect("writing");
    }

    let mut listed: Vec<String> = vault
        .list(scope.prefix())
        .expect("listing")
        .into_iter()
        .map(|entry| entry.target)
        .collect();
    listed.sort();

    assert_eq!(listed, vec![scope.target("alpha"), scope.target("beta"), scope.target("gamma")]);
}

#[test]
fn listing_a_prefix_with_nothing_under_it_is_empty_rather_than_an_error() {
    // What a fresh installation looks like, and Windows reports it as
    // ERROR_NOT_FOUND rather than as an empty list.
    let vault = WindowsVault;
    let empty = vault.list("DivinePulse/test-nothing-is-here/").expect("listing should succeed");
    assert!(empty.is_empty(), "{empty:?}");
}
