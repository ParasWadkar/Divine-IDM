//! What a saved password is *for*, and what this app may touch.
//!
//! [`idm_core::win::credentials`] knows how to put bytes in Windows'
//! vault and nothing else. This is the half that decides what goes in it:
//! what each entry is called, which of them are ours, and what is refused.
//! The split matters because the vault is shared with every other program
//! on the machine — the user's mail, their VPN, their bank — and the only
//! thing keeping this app away from those is the naming done here.
//!
//! # No client ever names a target
//!
//! The requests take a [`CredentialScope`], not a string. A request that
//! carried a raw target could carry `Microsoft/OneDrive` as easily as one
//! of ours, and then read, replace or delete it. Every target is derived
//! here from a vetted URL or a vetted proxy address, so the set of names
//! this program can reach is exactly the set it writes.
//!
//! # One password per origin, not per realm
//!
//! An HTTP challenge names a realm, and keying on it would be the literal
//! reading of the specification. It is the wrong one for a person. A realm
//! is an opaque string chosen by the server, which the user never sees and
//! cannot know in advance — so keying on it would mean nobody could save a
//! password until after a download had already failed, and then only for
//! the realm that happened to fail. Keying on the origin lets somebody put
//! in a password for a site they know wants one, before they need it.
//!
//! The cost, stated plainly: a site serving two realms from one origin
//! gets one saved password between them. That is rare, and the failure is
//! a visible 401 rather than anything silent.

use idm_core::win::credentials::{Credential, Secret, Vault, VaultError};
use idm_ipc::{CredentialScope, CredentialView};
use idm_net::Url;

/// Every target this app writes begins with this.
///
/// Also what it enumerates by, so nothing outside it is ever read. Changing
/// it orphans saved passwords rather than losing them — they stay in
/// Control Panel under the old name, where somebody can see and delete
/// them, which is the right failure for a rename nobody meant to do.
pub const PREFIX: &str = "DivinePulse/";

/// Turns a URL into the origin a password is saved against.
///
/// Scheme, host and port, with the port always written out. Without that,
/// `https://example.com` and `https://example.com:443` are the same site
/// and two different entries, and which one a download finds depends on
/// how the address happened to be typed.
///
/// The host is lower-cased because DNS is case-insensitive and a person
/// typing `Example.COM` means the same site. Nothing else is folded: path,
/// query and fragment are dropped rather than normalised, since none of
/// them is part of who is being signed in to.
pub fn origin_of(url: &str) -> Result<String, String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err("a site needs an address before a password can be saved for it".into());
    }
    let Ok(parsed) = Url::parse(trimmed) else {
        return Err(format!("{trimmed} is not an address this understands"));
    };
    // http and https only. A password for an `ftp://` or a `file://` is
    // either meaningless or a different mechanism entirely, and accepting
    // one here would put an entry in the vault that nothing reads.
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!(
            "{} is not a scheme that asks for a password this way; use http or https",
            parsed.scheme()
        ));
    }
    let Some(host) = parsed.host_str().filter(|host| !host.is_empty()) else {
        return Err(format!("{trimmed} names no site"));
    };
    // `port_or_known_default` fills in 80 and 443, which is the whole point
    // of writing the port out.
    let Some(port) = parsed.port_or_known_default() else {
        return Err(format!("{trimmed} names no port and has no usual one"));
    };
    Ok(format!("{}://{}:{port}", parsed.scheme(), host.to_ascii_lowercase()))
}

/// The vault target for a scope, and the line that describes it.
///
/// Returned together because they are derived from the same vetting, and
/// deriving them apart is how a listing comes to disagree with what it
/// removes.
pub fn target_for(scope: &CredentialScope) -> Result<(String, String), String> {
    match scope {
        CredentialScope::Site { url } => {
            let origin = origin_of(url)?;
            Ok((format!("{PREFIX}http/{origin}"), origin))
        }
        CredentialScope::Proxy { address } => {
            // The same vetting the proxy setting itself gets, so a password
            // cannot be saved against an address the proxy code would go on
            // to refuse -- which would leave an entry nothing ever reads.
            let address = crate::proxy::vet(address)?;
            Ok((format!("{PREFIX}proxy/{address}"), address))
        }
    }
}

/// Reads a target back into the scope it came from.
///
/// So that a listing can offer to remove an entry without the window
/// holding a raw target, and so an entry left by an older version under a
/// name this build no longer makes is simply not listed rather than shown
/// as something that cannot be removed.
fn scope_of(target: &str) -> Option<CredentialScope> {
    let rest = target.strip_prefix(PREFIX)?;
    if let Some(origin) = rest.strip_prefix("http/") {
        Some(CredentialScope::Site { url: origin.to_owned() })
    } else {
        rest.strip_prefix("proxy/")
            .map(|address| CredentialScope::Proxy { address: address.to_owned() })
    }
}

fn say(error: VaultError) -> String {
    error.to_string()
}

/// What is saved, without any of it being a password.
pub fn views(vault: &dyn Vault) -> Result<Vec<CredentialView>, String> {
    let mut entries: Vec<CredentialView> = vault
        .list(PREFIX)
        .map_err(say)?
        .into_iter()
        .filter_map(|entry| {
            // An entry under our prefix whose shape this build does not
            // recognise is skipped rather than guessed at. It stays in
            // Control Panel, where it is visible and removable.
            let scope = scope_of(&entry.target)?;
            let label = match &scope {
                CredentialScope::Site { url } => format!("Site — {url}"),
                CredentialScope::Proxy { address } => format!("Proxy — {address}"),
            };
            Some(CredentialView { scope, username: entry.username, label })
        })
        .collect();
    // A stable order, so the list does not reshuffle itself between two
    // openings of the same panel.
    entries.sort_by(|a, b| a.label.cmp(&b.label));
    Ok(entries)
}

/// Saves a password, replacing whatever was saved for the same thing.
pub fn set(
    vault: &dyn Vault,
    scope: &CredentialScope,
    username: &str,
    password: &str,
) -> Result<(), String> {
    let (target, _) = target_for(scope)?;
    let username = username.trim();
    if username.is_empty() {
        // Refused rather than stored blank: a credential with no username
        // cannot be used for Basic or Digest, so it would sit in the vault
        // looking like a saved password and never work.
        return Err("a saved password needs a username to go with it".into());
    }
    if password.is_empty() {
        // The way to have no password is to remove the entry, which says so
        // in the list. An empty one would be shown as saved.
        return Err("to save no password, remove the entry instead".into());
    }
    vault.write(&target, username, &Secret::new(password)).map_err(say)
}

/// Forgets one. Forgetting what is not there succeeds.
pub fn remove(vault: &dyn Vault, scope: &CredentialScope) -> Result<(), String> {
    let (target, _) = target_for(scope)?;
    vault.remove(&target).map_err(say)
}

/// The password for a site, if one was saved.
///
/// Called where a request is being built and nowhere else. A failure to
/// reach the vault is reported as "no password" rather than as an error:
/// the caller's next step either way is to try without one and let the
/// server say whether that was enough.
pub fn for_url(vault: &dyn Vault, url: &str) -> Option<Credential> {
    let target = format!("{PREFIX}http/{}", origin_of(url).ok()?);
    match vault.read(&target) {
        Ok(found) => found,
        Err(error) => {
            tracing::warn!(%error, "could not read a saved password");
            None
        }
    }
}

/// The password for a proxy, if one was saved.
pub fn for_proxy(vault: &dyn Vault, address: &str) -> Option<Credential> {
    let target = format!("{PREFIX}proxy/{}", crate::proxy::vet(address).ok()?);
    match vault.read(&target) {
        Ok(found) => found,
        Err(error) => {
            tracing::warn!(%error, "could not read a saved proxy password");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use idm_core::win::credentials::Entry;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeVault {
        items: Mutex<BTreeMap<String, Credential>>,
    }

    impl Vault for FakeVault {
        fn write(&self, target: &str, username: &str, secret: &Secret) -> Result<(), VaultError> {
            self.items.lock().unwrap().insert(
                target.to_owned(),
                Credential { username: username.to_owned(), secret: secret.clone() },
            );
            Ok(())
        }

        fn read(&self, target: &str) -> Result<Option<Credential>, VaultError> {
            Ok(self.items.lock().unwrap().get(target).cloned())
        }

        fn remove(&self, target: &str) -> Result<(), VaultError> {
            self.items.lock().unwrap().remove(target);
            Ok(())
        }

        fn list(&self, prefix: &str) -> Result<Vec<Entry>, VaultError> {
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

    fn site(url: &str) -> CredentialScope {
        CredentialScope::Site { url: url.to_owned() }
    }

    #[test]
    fn the_same_site_written_two_ways_is_one_entry() {
        // The whole reason the port is written out. Otherwise which saved
        // password a download finds depends on how the address was typed.
        for pair in [
            ("https://example.com", "https://example.com:443/some/path?q=1"),
            ("http://example.com", "http://EXAMPLE.com:80"),
            ("https://Example.COM/a", "https://example.com"),
        ] {
            assert_eq!(
                origin_of(pair.0).unwrap(),
                origin_of(pair.1).unwrap(),
                "{} and {} should be the same site",
                pair.0,
                pair.1,
            );
        }
    }

    #[test]
    fn two_different_sites_stay_different() {
        // The other direction, which is the one that matters for safety: a
        // password for one host must never be offered to another.
        let distinct = [
            "https://example.com",
            "http://example.com",
            "https://example.com:8443",
            "https://other.example.com",
            "https://example.org",
        ];
        for (index, one) in distinct.iter().enumerate() {
            for other in &distinct[index + 1..] {
                assert_ne!(origin_of(one).unwrap(), origin_of(other).unwrap(), "{one} vs {other}");
            }
        }
    }

    #[test]
    fn a_scheme_that_does_not_sign_in_this_way_is_refused() {
        // An entry for one of these would sit in the vault being read by
        // nothing, looking to the user like a password that is set up.
        for url in ["ftp://example.com", "file:///c:/x", "magnet:?xt=1", "sftp://example.com"] {
            assert!(origin_of(url).is_err(), "{url} should be refused");
        }
    }

    #[test]
    fn a_target_is_never_taken_from_the_caller() {
        // The property that keeps this program away from everything else in
        // the vault. The scope is a URL; the name is derived. There is no
        // input that makes `target_for` produce a name outside the prefix.
        let (target, _) = target_for(&site("https://example.com/download/a.bin")).unwrap();
        assert_eq!(target, "DivinePulse/http/https://example.com:443");
        assert!(target.starts_with(PREFIX));

        // Including when the URL is doing its best to look like one.
        let (sneaky, _) =
            target_for(&site("https://example.com/../../Microsoft/OneDrive")).unwrap();
        assert_eq!(sneaky, "DivinePulse/http/https://example.com:443");
    }

    #[test]
    fn a_proxy_password_is_keyed_by_the_vetted_address() {
        let (target, label) =
            target_for(&CredentialScope::Proxy { address: "  http://127.0.0.1:3128  ".into() })
                .unwrap();
        assert_eq!(target, "DivinePulse/proxy/http://127.0.0.1:3128");
        assert_eq!(label, "http://127.0.0.1:3128", "the trimmed form, as the proxy code sees it");
    }

    #[test]
    fn a_proxy_address_the_proxy_code_would_refuse_is_refused_here_too() {
        // Otherwise a password is saved against an address no download will
        // ever use, and the list shows it as set up.
        let scope = CredentialScope::Proxy { address: "not-an-address".into() };
        assert!(target_for(&scope).is_err());
        // And the one that carries its own password, which is the thing
        // this whole feature exists to replace.
        let scope = CredentialScope::Proxy { address: "http://me:pw@127.0.0.1:3128".into() };
        assert!(target_for(&scope).is_err());
    }

    #[test]
    fn a_saved_password_comes_back_for_the_site_it_was_saved_for() {
        let vault = FakeVault::default();
        set(&vault, &site("https://example.com"), "me", "hunter2").unwrap();

        // Found however the address is written, which is the point of
        // canonicalising before the lookup.
        let found =
            for_url(&vault, "https://EXAMPLE.com:443/file.bin").expect("it should be found");
        assert_eq!(found.username, "me");
        assert_eq!(found.secret.expose(), "hunter2");

        // And not offered to anybody else.
        assert!(for_url(&vault, "https://evil.example.com").is_none());
        assert!(
            for_url(&vault, "http://example.com").is_none(),
            "a different scheme is a different site"
        );
    }

    #[test]
    fn a_proxy_password_comes_back_for_the_address_it_was_saved_for() {
        let vault = FakeVault::default();
        let proxy = CredentialScope::Proxy { address: "http://127.0.0.1:3128".into() };
        set(&vault, &proxy, "me", "hunter2").unwrap();

        // Found however it was typed, because the lookup vets the address
        // the same way saving it did.
        let found = for_proxy(&vault, "  http://127.0.0.1:3128 ").expect("it should be found");
        assert_eq!(found.username, "me");
        assert_eq!(found.secret.expose(), "hunter2");

        // And not handed to a different proxy.
        assert!(for_proxy(&vault, "http://127.0.0.1:8080").is_none());
    }

    #[test]
    fn a_proxy_password_is_not_a_site_password() {
        // Two different secrets with two different jobs. Offering one where
        // the other was meant would send the proxy's password to every host
        // downloaded through it, or the site's to whoever runs the proxy.
        let vault = FakeVault::default();
        set(&vault, &CredentialScope::Proxy { address: "http://127.0.0.1:3128".into() }, "p", "pp")
            .unwrap();
        set(&vault, &site("https://example.com"), "s", "ss").unwrap();

        assert_eq!(for_proxy(&vault, "http://127.0.0.1:3128").unwrap().secret.expose(), "pp");
        assert_eq!(for_url(&vault, "https://example.com").unwrap().secret.expose(), "ss");
        // A proxy address is not a site, and a site is not a proxy.
        assert!(for_url(&vault, "http://127.0.0.1:3128").is_none());
        assert!(for_proxy(&vault, "https://example.com").is_none());
    }

    #[test]
    fn listing_says_what_each_one_is_for_and_never_the_password() {
        let vault = FakeVault::default();
        set(&vault, &site("https://example.com"), "me", "hunter2").unwrap();
        set(&vault, &CredentialScope::Proxy { address: "http://127.0.0.1:3128".into() }, "p", "s")
            .unwrap();

        let listed = views(&vault).unwrap();
        assert_eq!(listed.len(), 2);
        let shown = format!("{listed:?}");
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(listed.iter().any(|view| view.label.starts_with("Proxy — ")), "{listed:?}");
        assert!(listed.iter().any(|view| view.label.starts_with("Site — ")), "{listed:?}");
    }

    #[test]
    fn listing_leaves_alone_anything_that_is_not_ours() {
        // Belt and braces: the vault filters by prefix, and this filters
        // again by shape. An entry that is neither is not listed, and so
        // cannot be offered for removal through this app.
        let vault = FakeVault::default();
        vault.write("Microsoft/OneDrive", "them", &Secret::new("x")).unwrap();
        vault.write("DivinePulse/something-else/x", "them", &Secret::new("x")).unwrap();
        set(&vault, &site("https://example.com"), "me", "hunter2").unwrap();

        let listed = views(&vault).unwrap();
        assert_eq!(listed.len(), 1, "{listed:?}");
    }

    #[test]
    fn what_was_listed_is_what_can_be_removed() {
        // The round trip that makes the window's Remove button work without
        // it ever holding a raw target: list, hand the scope back, remove.
        let vault = FakeVault::default();
        set(&vault, &site("https://example.com/a/b"), "me", "hunter2").unwrap();

        let listed = views(&vault).unwrap();
        let only = listed.first().expect("one entry");
        remove(&vault, &only.scope).unwrap();

        assert!(views(&vault).unwrap().is_empty());
        assert!(for_url(&vault, "https://example.com").is_none());
    }

    #[test]
    fn saving_again_replaces_rather_than_adding() {
        let vault = FakeVault::default();
        set(&vault, &site("https://example.com"), "old", "one").unwrap();
        set(&vault, &site("https://example.com/elsewhere"), "new", "two").unwrap();

        assert_eq!(views(&vault).unwrap().len(), 1, "the same origin is the same entry");
        let found = for_url(&vault, "https://example.com").unwrap();
        assert_eq!(found.username, "new");
        assert_eq!(found.secret.expose(), "two");
    }

    #[test]
    fn a_blank_username_or_password_is_refused_in_words() {
        let vault = FakeVault::default();
        let error = set(&vault, &site("https://example.com"), "  ", "x").unwrap_err();
        assert!(error.contains("username"), "{error}");
        // An empty password would show in the list as saved. Removing the
        // entry is how to have none, and it says so.
        let error = set(&vault, &site("https://example.com"), "me", "").unwrap_err();
        assert!(error.contains("remove the entry"), "{error}");
        assert!(views(&vault).unwrap().is_empty(), "neither should have been stored");
    }

    #[test]
    fn forgetting_something_that_was_never_saved_succeeds() {
        let vault = FakeVault::default();
        remove(&vault, &site("https://example.com")).expect("removing nothing should succeed");
    }
}
