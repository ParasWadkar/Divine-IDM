//! Vetting a proxy address, and turning the setting into a client's idea of
//! one.
//!
//! Checked while the person is still at the screen rather than on the first
//! download, for the same reason the download folder is: a proxy that turns
//! out to be unusable otherwise surfaces as a download failing for reasons
//! that look nothing like "the address you typed has a typo in it".

use idm_ipc::{ProxySetting, Settings};
use idm_net::Url;

/// Re-exported so callers can name what [`mode`] returns without taking a
/// dependency on the transport crate. The window does exactly that.
pub use idm_net::client::ProxyMode;

/// Schemes a proxy address may use.
///
/// `socks5h` differs from `socks5` only in who resolves the hostname — the
/// proxy rather than this machine — which is the one people actually want
/// when the proxy is a tunnel to somewhere that can see names from which
/// this machine is cut off.
const SCHEMES: [&str; 4] = ["http", "https", "socks5", "socks5h"];

/// Checks a proxy address, returning it tidied.
///
/// The errors are written to be read by whoever typed it, because that is
/// who will see them.
pub fn vet(address: &str) -> Result<String, String> {
    let trimmed = address.trim();
    if trimmed.is_empty() {
        return Err("a proxy needs an address, such as http://127.0.0.1:3128".into());
    }

    let Ok(parsed) = Url::parse(trimmed) else {
        return Err(format!(
            "{trimmed} is not an address this understands; it needs a scheme, as in \
             http://127.0.0.1:3128 or socks5://127.0.0.1:1080"
        ));
    };

    if !SCHEMES.contains(&parsed.scheme()) {
        return Err(format!(
            "{} is not a proxy scheme this knows; use one of {}",
            parsed.scheme(),
            SCHEMES.join(", ")
        ));
    }

    if parsed.host_str().is_none_or(str::is_empty) {
        return Err(format!("{trimmed} names no host to connect to"));
    }

    // Still refused, and for the same reason as before: this string is kept
    // in `settings.json` in the clear, so taking a password here would put
    // one in a plain file the user has no reason to think is holding it.
    //
    // What has changed is that there is now somewhere for it to go. An
    // authenticated proxy works -- the password lives in Windows Credential
    // Manager under this address, and `credentials::for_proxy` reads it at
    // the moment a client is built. So this says where to put it rather
    // than that it cannot be done.
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("a proxy address must not carry a username or password: this file is kept \
                    in the clear. Put the address alone here, then save the password under \
                    Settings -> Saved passwords, where Windows keeps it"
            .into());
    }

    Ok(trimmed.to_owned())
}

/// What these settings mean to an HTTP client.
pub fn mode(settings: &Settings) -> ProxyMode {
    match settings.proxy {
        ProxySetting::System => ProxyMode::System,
        ProxySetting::Direct => ProxyMode::Direct,
        ProxySetting::Manual => match vet(&settings.proxy_address) {
            Ok(address) => ProxyMode::Manual(address),
            // A settings file edited by hand can still hold nonsense, and
            // this runs long after anybody could be told about it. Going
            // straight out is the safer reading of "not the system one, and
            // nothing usable given": falling back to `System` would quietly
            // start proxying through whatever the environment named, which
            // is the one thing choosing `Manual` rules out.
            Err(_) => ProxyMode::Direct,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(proxy: ProxySetting, address: &str) -> Settings {
        Settings { proxy, proxy_address: address.to_owned(), ..Settings::default() }
    }

    #[test]
    fn the_schemes_that_mean_something_are_accepted() {
        for address in [
            "http://127.0.0.1:3128",
            "https://proxy.example.test:8080",
            "socks5://127.0.0.1:1080",
            "socks5h://127.0.0.1:1080",
        ] {
            assert_eq!(vet(address).as_deref(), Ok(address), "{address} should be accepted");
        }
    }

    #[test]
    fn surrounding_space_comes_off() {
        assert_eq!(vet("  http://127.0.0.1:3128  ").as_deref(), Ok("http://127.0.0.1:3128"));
    }

    #[test]
    fn an_address_with_no_scheme_says_what_one_looks_like() {
        // What somebody types first, every time.
        let error = vet("127.0.0.1:3128").unwrap_err();
        assert!(error.contains("http://"), "{error}");
    }

    #[test]
    fn a_scheme_that_is_not_a_proxy_scheme_is_refused() {
        let error = vet("ftp://127.0.0.1:3128").unwrap_err();
        assert!(error.contains("socks5"), "the ones it does know should be listed: {error}");
    }

    #[test]
    fn the_refusal_says_where_the_password_should_go_instead() {
        // The difference this commit makes. Before, the honest answer was
        // that authenticated proxies did not work; now they do, and a
        // refusal that only said "no" would be sending somebody away from
        // a feature that is sitting right there.
        let error = vet("http://someone:hunter2@127.0.0.1:3128").unwrap_err();
        assert!(error.contains("Saved passwords"), "it should say where: {error}");
        assert!(!error.contains("not supported"), "they are supported now: {error}");
    }

    #[test]
    fn credentials_in_the_address_are_refused_and_the_reason_given() {
        // Not because an authenticated proxy cannot work -- it does now --
        // but because accepting one *here* writes a password into
        // settings.json, which is a plain file nobody expects to hold one.
        // The password goes in the vault instead, keyed by this address.
        for address in [
            "http://someone:hunter2@127.0.0.1:3128",
            "http://someone@127.0.0.1:3128",
            "socks5://someone:hunter2@127.0.0.1:1080",
        ] {
            let error = vet(address).unwrap_err();
            assert!(error.contains("kept in the clear"), "{address}: {error}");
        }
    }

    #[test]
    fn an_empty_address_asks_for_one() {
        assert!(vet("").unwrap_err().contains("needs an address"));
        assert!(vet("   ").unwrap_err().contains("needs an address"));
    }

    #[test]
    fn the_default_is_to_honour_the_machines_own_configuration() {
        assert_eq!(mode(&Settings::default()), ProxyMode::System);
    }

    #[test]
    fn direct_means_direct() {
        // Including when the environment has a proxy in it, which is the
        // whole reason this choice exists.
        assert_eq!(mode(&with(ProxySetting::Direct, "")), ProxyMode::Direct);
        assert_eq!(
            mode(&with(ProxySetting::Direct, "http://127.0.0.1:3128")),
            ProxyMode::Direct,
            "a leftover address must not override an explicit Direct"
        );
    }

    #[test]
    fn a_vetted_manual_address_is_used() {
        assert_eq!(
            mode(&with(ProxySetting::Manual, "socks5://127.0.0.1:1080")),
            ProxyMode::Manual("socks5://127.0.0.1:1080".to_owned())
        );
    }

    #[test]
    fn manual_with_nothing_usable_goes_straight_out_rather_than_to_the_environment() {
        // Falling back to System here would proxy through whatever
        // HTTP_PROXY named -- the one outcome choosing Manual rules out.
        for address in ["", "not a url", "ftp://127.0.0.1:3128", "http://u:p@127.0.0.1:3128"] {
            assert_eq!(
                mode(&with(ProxySetting::Manual, address)),
                ProxyMode::Direct,
                "{address} should fall back to Direct, never System"
            );
        }
    }
}
