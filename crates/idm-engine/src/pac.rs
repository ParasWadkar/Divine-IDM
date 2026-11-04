//! Asking Windows which proxy to use, for networks that decide per address.
//!
//! On a managed network the answer to "which proxy?" is not a setting, it
//! is a small program: a PAC script, fetched from somewhere on the LAN,
//! whose `FindProxyForURL` is evaluated for every address. WPAD is the same
//! thing with the script's location discovered over DHCP and DNS rather
//! than typed in.
//!
//! # WinHTTP rather than a JavaScript engine
//!
//! The argument is not that JavaScript is hard. A PAC file's dialect is a
//! fixed host API — `dnsDomainIs`, `shExpMatch`, `isInNet`, `myIpAddress`
//! and six more — which would have to be implemented *on top of* whatever
//! engine was embedded, so the engine buys only the expression evaluator
//! and none of the work. WinHTTP brings the host functions, the script
//! caching, and WPAD discovery over DHCP and DNS, which an embedded engine
//! contributes nothing toward.
//!
//! Two things settle it beyond convenience. It resolves to the same answer
//! every other program on the machine gets, which is the actual requirement
//! on a network where a proxy is mandatory and a wrong answer means no
//! downloads at all. And it avoids running network-supplied JavaScript
//! inside this process — a PAC file is code fetched from a host this
//! program did not choose.
//!
//! # Resolved here, not in the transport
//!
//! `ProxyMode` stays closed and stays *resolved*: idm-net is handed an
//! address or told to go direct, and never learns that a script was
//! involved. So the transport crate stays free of Windows, and the
//! invariant that `ProxyMode` is translated only by this layer holds.
//!
//! PAC is per-URL by definition, so this is evaluated once per download
//! rather than once per request — a download's segments all go to one
//! origin, so the answer cannot differ between them.

use idm_net::client::ProxyMode;

/// Where the script comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoConfig {
    /// Find it the way the rest of the machine does: DHCP, then DNS.
    Discover,
    /// Fetch it from here.
    Script(String),
}

/// Picks a usable proxy out of what WinHTTP hands back.
///
/// The string is a list, separated by semicolons or spaces, of entries that
/// may carry a scheme prefix: `proxy:80`, `http=proxy:80;https=secure:443`,
/// `socks=socks-host:1080`. The first entry this understands wins, which
/// matches how every other client treats the list — the rest are fallbacks
/// for when the first is unreachable, and this engine finds that out by
/// failing rather than by probing each in turn.
///
/// Returns an address with a scheme on it, because that is what the
/// transport needs and what distinguishes an HTTP proxy from a SOCKS one.
pub fn pick(list: &str) -> Option<String> {
    for entry in list.split([';', ' ', '\t', '\n']) {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }

        // `scheme=host:port`, where the scheme says which kind of request
        // the entry is for rather than what the proxy speaks. Split before
        // looking for a port, or `http=host:80` loses its host.
        let (prefix, target) = match entry.split_once('=') {
            Some((prefix, target)) => (Some(prefix.trim().to_ascii_lowercase()), target.trim()),
            None => (None, entry),
        };

        if target.is_empty() {
            continue;
        }
        // Already a URL, which WinHTTP does sometimes produce.
        if let Some(scheme) = ["http://", "https://", "socks5://", "socks5h://"]
            .into_iter()
            .find(|scheme| target.to_ascii_lowercase().starts_with(scheme))
        {
            let _ = scheme;
            return Some(target.to_owned());
        }

        // A bare `host:port`. The scheme to put on it comes from the
        // prefix when there is one: a `socks=` entry is a SOCKS proxy and
        // sending HTTP to it would fail in a way that looks like the proxy
        // being down.
        let scheme = match prefix.as_deref() {
            Some("socks") => "socks5://",
            // `https=` means "use this proxy for https requests", not
            // "speak TLS to the proxy", so it is still an HTTP proxy.
            Some("http") | Some("https") | Some("ftp") | None => "http://",
            // Something unrecognised. Skipped rather than guessed at: the
            // list usually holds another entry that is plain.
            Some(_) => continue,
        };
        return Some(format!("{scheme}{target}"));
    }
    None
}

/// Asks Windows which proxy to use for this address.
///
/// Blocking underneath — it may fetch a script and, for `Discover`, send
/// DHCP and DNS queries — so it runs on a blocking worker. The first call
/// on a network without WPAD is the slow one, and it is bounded by
/// WinHTTP's own timeouts rather than by anything here.
///
/// Every failure resolves to [`ProxyMode::Direct`] rather than to an error.
/// The alternative readings are both worse: `System` would quietly start
/// proxying through whatever the environment named, which is the one thing
/// choosing automatic configuration rules out, and failing the download
/// would turn a momentarily unreachable script into a dead application.
pub async fn resolve(config: AutoConfig, url: &str) -> ProxyMode {
    let url = url.to_owned();
    match tokio::task::spawn_blocking(move || platform::for_url(&config, &url)).await {
        Ok(Some(address)) => ProxyMode::Manual(address),
        Ok(None) => ProxyMode::Direct,
        Err(error) => {
            tracing::warn!(%error, "the proxy script lookup did not finish");
            ProxyMode::Direct
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::{AutoConfig, pick};

    use windows::Win32::Networking::WinHttp::{
        WINHTTP_ACCESS_TYPE_NO_PROXY, WINHTTP_AUTO_DETECT_TYPE_DHCP,
        WINHTTP_AUTO_DETECT_TYPE_DNS_A, WINHTTP_AUTOPROXY_AUTO_DETECT,
        WINHTTP_AUTOPROXY_CONFIG_URL, WINHTTP_AUTOPROXY_OPTIONS, WINHTTP_PROXY_INFO,
        WinHttpCloseHandle, WinHttpGetProxyForUrl, WinHttpOpen,
    };
    use windows::core::{PCWSTR, PWSTR};

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Reads and frees a string WinHTTP allocated.
    ///
    /// # Safety
    ///
    /// `value` must be null or a NUL-terminated buffer from WinHTTP, which
    /// owns it until freed here.
    unsafe fn take(value: PWSTR) -> Option<String> {
        if value.is_null() {
            return None;
        }
        // SAFETY: the caller guarantees termination; `to_string` copies, so
        // nothing borrows the allocation past the free below.
        let text = unsafe { value.to_string().ok() };
        // SAFETY: WinHTTP documents these as the caller's to release, and
        // GlobalFree is what it documents releasing them with.
        unsafe {
            let _ = windows::Win32::Foundation::GlobalFree(Some(
                windows::Win32::Foundation::HGLOBAL(value.0.cast()),
            ));
        }
        text
    }

    pub(super) fn for_url(config: &AutoConfig, url: &str) -> Option<String> {
        // A session per lookup rather than one kept for the process. WinHTTP
        // caches the fetched script against the session handle, so a kept
        // one would be faster -- and would also mean a PAC file edited on
        // the network never took effect until the app restarted, and a raw
        // handle living in a static. A download is not frequent enough for
        // the fetch to matter.
        //
        // SAFETY: all-null arguments are the documented form for a session
        // that does no requests of its own.
        let session = unsafe {
            WinHttpOpen(
                PCWSTR(wide(concat!("DivinePulse/", env!("CARGO_PKG_VERSION"))).as_ptr()),
                WINHTTP_ACCESS_TYPE_NO_PROXY,
                PCWSTR::null(),
                PCWSTR::null(),
                0,
            )
        };
        if session.is_null() {
            tracing::warn!("could not open a WinHTTP session to read the proxy script");
            return None;
        }

        let script = match config {
            AutoConfig::Script(url) => Some(wide(url)),
            AutoConfig::Discover => None,
        };

        let options = WINHTTP_AUTOPROXY_OPTIONS {
            dwFlags: match config {
                AutoConfig::Discover => WINHTTP_AUTOPROXY_AUTO_DETECT,
                AutoConfig::Script(_) => WINHTTP_AUTOPROXY_CONFIG_URL,
            },
            // Both discovery methods, which is what every other client on
            // the machine asks for. DHCP first is WinHTTP's own order.
            dwAutoDetectFlags: match config {
                AutoConfig::Discover => {
                    WINHTTP_AUTO_DETECT_TYPE_DHCP | WINHTTP_AUTO_DETECT_TYPE_DNS_A
                }
                AutoConfig::Script(_) => 0,
            },
            lpszAutoConfigUrl: script
                .as_ref()
                .map_or(PCWSTR::null(), |script| PCWSTR(script.as_ptr())),
            // Let WinHTTP answer a challenge from the script's own host
            // with this user's credentials. A PAC served from an
            // authenticated intranet share is otherwise simply unreadable,
            // and the download would go direct on a network where that is
            // not allowed.
            fAutoLogonIfChallenged: true.into(),
            ..Default::default()
        };

        let mut info = WINHTTP_PROXY_INFO::default();
        // Bound to a local: `wide(url)` as an argument would be dropped at
        // the end of the call expression, which is not obviously after
        // WinHTTP has read it.
        let target = wide(url);
        let mut options = options;
        // SAFETY: `session` is live, the wide strings outlive the call, and
        // `info` is a valid out-parameter whose strings are freed below.
        let outcome = unsafe {
            WinHttpGetProxyForUrl(session, PCWSTR(target.as_ptr()), &mut options, &mut info)
        };

        // SAFETY: closing a handle this function opened, once.
        let _ = unsafe { WinHttpCloseHandle(session) };

        if outcome.is_err() {
            // Not an error worth failing a download over: no script, an
            // unreachable one, or a script that said nothing applies.
            tracing::debug!("no proxy script answer for this address; going direct");
            return None;
        }

        // SAFETY: WinHTTP filled these and documents them as ours to free.
        let proxy = unsafe { take(info.lpszProxy) };
        // Freed even though it is unused: the bypass list is this process's
        // to release either way, and leaking it once per download adds up.
        let _ = unsafe { take(info.lpszProxyBypass) };

        // `NO_PROXY` is the script having said DIRECT, which is an answer
        // rather than a failure.
        if info.dwAccessType == WINHTTP_ACCESS_TYPE_NO_PROXY {
            return None;
        }
        proxy.as_deref().and_then(pick)
    }
}

#[cfg(not(windows))]
mod platform {
    use super::AutoConfig;

    pub(super) fn for_url(_config: &AutoConfig, _url: &str) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_host_and_port_gets_a_scheme() {
        // The transport needs one, and the commonest answer has none.
        assert_eq!(
            pick("proxy.example.test:3128").as_deref(),
            Some("http://proxy.example.test:3128")
        );
    }

    #[test]
    fn the_first_usable_entry_wins() {
        // The rest are fallbacks for when the first is unreachable, which
        // this engine discovers by failing rather than by probing.
        assert_eq!(
            pick("first.example.test:80;second.example.test:8080").as_deref(),
            Some("http://first.example.test:80"),
        );
        // Spaces separate them just as well as semicolons.
        assert_eq!(
            pick("first.example.test:80 second.example.test:8080").as_deref(),
            Some("http://first.example.test:80"),
        );
    }

    #[test]
    fn a_scheme_prefix_is_stripped_rather_than_taken_for_a_host() {
        // `http=host:80` split on the colon first would leave `http=host`
        // as the host and lose the port.
        assert_eq!(
            pick("http=proxy.example.test:80").as_deref(),
            Some("http://proxy.example.test:80")
        );
        // `https=` says which requests the entry is for, not that the proxy
        // speaks TLS: it is still an HTTP proxy.
        assert_eq!(
            pick("https=proxy.example.test:443").as_deref(),
            Some("http://proxy.example.test:443"),
        );
    }

    #[test]
    fn a_socks_entry_keeps_being_socks() {
        // Sending HTTP to a SOCKS proxy fails in a way that reads as the
        // proxy being down, which is a long way from the real cause.
        assert_eq!(
            pick("socks=socks.example.test:1080").as_deref(),
            Some("socks5://socks.example.test:1080")
        );
    }

    #[test]
    fn an_entry_that_is_already_a_url_is_left_alone() {
        for address in ["http://p.example.test:3128", "socks5h://p.example.test:1080"] {
            assert_eq!(pick(address).as_deref(), Some(address));
        }
    }

    #[test]
    fn an_unrecognised_prefix_is_skipped_for_one_that_is_not() {
        // Skipped rather than guessed at. The list nearly always carries a
        // plain entry too.
        assert_eq!(
            pick("gopher=old.example.test:70;plain.example.test:80").as_deref(),
            Some("http://plain.example.test:80"),
        );
    }

    #[test]
    fn nothing_usable_is_nothing_rather_than_a_wrong_guess() {
        assert_eq!(pick(""), None);
        assert_eq!(pick("   "), None);
        assert_eq!(pick(";; ;"), None);
        assert_eq!(pick("gopher=old.example.test:70"), None);
        // A prefix with no target behind it.
        assert_eq!(pick("http="), None);
    }

    #[tokio::test]
    async fn a_lookup_that_finds_nothing_goes_direct_rather_than_failing() {
        // The fallback that matters: a momentarily unreachable script must
        // not turn into a dead application, and must not fall back to the
        // environment's proxy -- which is the one thing choosing automatic
        // configuration rules out.
        let mode = resolve(
            AutoConfig::Script("http://127.0.0.1:1/nothing-is-here.pac".into()),
            "https://example.com/file.bin",
        )
        .await;
        assert_eq!(mode, ProxyMode::Direct);
    }
}
