//! Which browsers exist, and whether each is installed here.
//!
//! One table, used by two callers that must agree: the command line registers
//! the native messaging host with every browser in it, and the window lists
//! the ones that are installed so each can be switched on or off. If they kept
//! separate copies, a browser added to one would quietly be missing from the
//! other -- registered but never shown, or shown but never registered.
//!
//! There are only two extension codebases on Windows: one Chromium build and
//! one Gecko build. Every Chromium browser speaks the same extension API, and
//! so does every Firefox fork. The only thing that differs per browser is the
//! registry key it reads native messaging hosts from, which is why this is a
//! table and a loop rather than a family of implementations.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Chromium,
    Gecko,
}

/// One browser, and where it looks for native messaging hosts.
#[derive(Debug, Clone, Copy)]
pub struct Browser {
    pub label: &'static str,
    /// Registry path under `Software\`, excluding the `NativeMessagingHosts`
    /// leaf — e.g. `Google\Chrome`.
    pub root: &'static str,
    pub family: Family,
}

/// Known browsers. Adding one is a single line.
pub const BROWSERS: &[Browser] = &[
    // --- Chromium family -------------------------------------------------
    Browser { label: "Google Chrome", root: r"Google\Chrome", family: Family::Chromium },
    Browser { label: "Chrome Beta", root: r"Google\Chrome Beta", family: Family::Chromium },
    Browser { label: "Chrome Dev", root: r"Google\Chrome Dev", family: Family::Chromium },
    Browser { label: "Chrome Canary", root: r"Google\Chrome SxS", family: Family::Chromium },
    Browser { label: "Chromium", root: "Chromium", family: Family::Chromium },
    Browser { label: "Microsoft Edge", root: r"Microsoft\Edge", family: Family::Chromium },
    Browser { label: "Edge Beta", root: r"Microsoft\Edge Beta", family: Family::Chromium },
    Browser { label: "Edge Dev", root: r"Microsoft\Edge Dev", family: Family::Chromium },
    Browser { label: "Brave", root: r"BraveSoftware\Brave-Browser", family: Family::Chromium },
    Browser {
        label: "Brave Beta",
        root: r"BraveSoftware\Brave-Browser-Beta",
        family: Family::Chromium,
    },
    Browser { label: "Vivaldi", root: "Vivaldi", family: Family::Chromium },
    Browser { label: "Opera", root: r"Opera Software\Opera Stable", family: Family::Chromium },
    Browser {
        label: "Opera GX",
        root: r"Opera Software\Opera GX Stable",
        family: Family::Chromium,
    },
    Browser { label: "Yandex Browser", root: r"Yandex\YandexBrowser", family: Family::Chromium },
    Browser { label: "Arc", root: r"TheBrowserCompany\Arc", family: Family::Chromium },
    Browser { label: "Coc Coc", root: r"CocCoc\Browser", family: Family::Chromium },
    Browser { label: "Thorium", root: "Thorium", family: Family::Chromium },
    // --- Gecko family ----------------------------------------------------
    // Most Firefox forks read Mozilla's key, but the ones that use their own
    // are listed so they work without the user having to know that.
    Browser { label: "Firefox (and forks)", root: "Mozilla", family: Family::Gecko },
    Browser { label: "LibreWolf", root: "LibreWolf", family: Family::Gecko },
    Browser { label: "Waterfox", root: "Waterfox", family: Family::Gecko },
    Browser { label: "Zen Browser", root: "Zen", family: Family::Gecko },
];

impl std::str::FromStr for Family {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        // The aliases are the words someone reaches for. Nobody thinks of
        // Chrome as "Chromium family" when they are trying to register
        // Thorium, and refusing "chrome" to insist on "chromium" would be
        // pedantry in the one place the user is already improvising.
        match text.to_ascii_lowercase().as_str() {
            "chromium" | "chrome" | "blink" => Ok(Self::Chromium),
            "gecko" | "firefox" | "mozilla" => Ok(Self::Gecko),
            other => {
                Err(format!("unknown browser family {other:?}: expected `chromium` or `gecko`"))
            }
        }
    }
}

impl Browser {
    /// What to call this browser to a person: "Chrome", not "Google Chrome".
    ///
    /// The table's labels are for the command line, where the full product
    /// name is clearer. In a list of three browsers the vendor is noise, and
    /// "(and forks)" is an explanation rather than a name.
    pub fn display_name(&self) -> String {
        self.label.replace("Google ", "").replace("Microsoft ", "").replace(" (and forks)", "")
    }

    /// The short name the extension reports itself under.
    ///
    /// The extension labels its pairing request "Chrome on this PC",
    /// "Brave on this PC", and so on, using one word for the browser. That
    /// word has to be matched back to a row in this table to say which
    /// browser a pairing belongs to, and the table's labels are longer
    /// ("Google Chrome", "Microsoft Edge", "Firefox (and forks)").
    pub fn matches_name(&self, name: &str) -> bool {
        let name = name.trim().to_ascii_lowercase();
        !name.is_empty() && self.label.to_ascii_lowercase().contains(&name)
    }
}

/// The browsers that look installed on this machine.
///
/// Only those: listing twenty-one browsers when two are present would bury
/// the ones that matter. The cost is that a browser this cannot detect is not
/// offered, which `--extra-root` exists to cover.
pub fn installed() -> Vec<Browser> {
    BROWSERS.iter().copied().filter(|browser| browser_present(browser.root)).collect()
}

#[cfg(windows)]
mod windows_detect {
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, RegCloseKey, RegEnumKeyExW,
        RegOpenKeyExW, RegQueryInfoKeyW,
    };
    use windows::core::{PCWSTR, PWSTR};

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Whether a browser appears installed.
    ///
    /// The obvious test -- does `Software\<root>` exist -- is wrong, and
    /// wrong in a way that hides itself: registering the host *creates*
    /// `Software\<root>\NativeMessagingHosts`, and so creates
    /// `Software\<root>` too. Every browser we ever register then reports as
    /// installed on every later run, including ones that were never on the
    /// machine. Observed exactly that: `HKCU\Software\Vivaldi` existed with
    /// `NativeMessagingHosts` as its only child, on a PC with no Vivaldi.
    ///
    /// So the evidence has to be something the browser itself wrote: any
    /// value, or any subkey that is not the one we create. Browsers write
    /// `BLBeacon`, `StabilityMetrics` and similar on first run.
    ///
    /// A key left behind by an uninstalled browser still reads as present.
    /// That is the harmless direction -- a registration nothing reads -- and
    /// `--all-browsers` exists for the opposite case.
    pub fn browser_present(root: &str) -> bool {
        let path = wide(&format!(r"Software\{root}"));
        [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE]
            .into_iter()
            .any(|hive| has_browser_written_content(hive, &path))
    }

    /// True if the key holds anything other than our own host registration.
    fn has_browser_written_content(hive: HKEY, path: &[u16]) -> bool {
        let mut key = HKEY::default();
        // SAFETY: `path` is a NUL-terminated wide string outliving the call;
        // `key` is a valid out-pointer.
        let status =
            unsafe { RegOpenKeyExW(hive, PCWSTR(path.as_ptr()), Some(0), KEY_READ, &raw mut key) };
        if status != ERROR_SUCCESS {
            return false;
        }

        let mut subkeys = 0u32;
        let mut values = 0u32;
        // SAFETY: `key` is open; every out-pointer is valid for the call.
        let queried = unsafe {
            RegQueryInfoKeyW(
                key,
                None,
                None,
                None,
                Some(&raw mut subkeys),
                None,
                None,
                Some(&raw mut values),
                None,
                None,
                None,
                None,
            )
        };

        let present = if queried != ERROR_SUCCESS {
            false
        } else if values > 0 || subkeys > 1 {
            // We write no values here and exactly one subkey, so anything
            // beyond that came from the browser.
            true
        } else if subkeys == 1 {
            !sole_subkey_is_ours(key)
        } else {
            false
        };

        // SAFETY: opened above and not used after this point.
        unsafe {
            let _ = RegCloseKey(key);
        }
        present
    }

    /// Whether the key's only child is the one `register` creates.
    fn sole_subkey_is_ours(key: HKEY) -> bool {
        // Long enough for any real key name; a longer one is not ours.
        let mut name = [0u16; 256];
        let mut len = name.len() as u32;
        // SAFETY: `key` is open; `name`/`len` describe one writable buffer.
        let status = unsafe {
            RegEnumKeyExW(
                key,
                0,
                Some(PWSTR(name.as_mut_ptr())),
                &raw mut len,
                None,
                None,
                None,
                None,
            )
        };
        if status != ERROR_SUCCESS {
            return false;
        }
        let found = String::from_utf16_lossy(&name[..len as usize]);
        found.eq_ignore_ascii_case("NativeMessagingHosts")
    }
}

#[cfg(windows)]
pub use windows_detect::browser_present;

#[cfg(not(windows))]
/// There is no registry to ask off Windows.
pub fn browser_present(_root: &str) -> bool {
    false
}

/// One line of the browser list in the window.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BrowserRow {
    /// What to call it: "Chrome".
    pub name: String,
    /// Stable identity for the hidden list and for finding the row again.
    pub key: String,
    /// `connected`, `off` or `not_connected`.
    ///
    /// Three states rather than two because "off" is not "not connected":
    /// an off browser is paired and one switch away from working, and
    /// showing it as unconnected would send the user off to reinstall an
    /// extension that is fine.
    pub state: &'static str,
    /// Every pairing that belongs to this browser. More than one when the
    /// same browser has several profiles, each with its own extension.
    pub client_ids: Vec<String>,
    /// `chromium` or `gecko`, which decides how its extension is installed.
    pub family: &'static str,
    pub hidden: bool,
}

fn family_name(family: Family) -> &'static str {
    match family {
        Family::Chromium => "chromium",
        Family::Gecko => "gecko",
    }
}

/// The one word a pairing label names its browser by.
///
/// The extension labels itself "Chrome on this PC". Taking that apart here
/// keeps the format in one place: if the extension's wording changes, this is
/// the line that notices.
fn browser_word(label: &str) -> &str {
    label.strip_suffix(" on this PC").unwrap_or(label).trim()
}

/// Builds the list the window shows.
///
/// Starts from the browsers that are installed, because those are the ones a
/// person might want to connect, then adds any browser that is paired but not
/// detected -- a portable copy, or one this machine cannot see -- so that a
/// working connection never disappears from view for lack of a registry key.
///
/// Hidden browsers are included and flagged rather than dropped, so the window
/// can offer to bring them back. Dropping them here would leave nothing to
/// restore from.
pub fn rows(
    installed: &[Browser],
    paired: &[idm_ipc::PairedClient],
    hidden: &[String],
) -> Vec<BrowserRow> {
    let is_hidden = |name: &str| hidden.iter().any(|h| h.trim().eq_ignore_ascii_case(name.trim()));

    let mut out: Vec<BrowserRow> = Vec::new();

    for browser in installed {
        let name = browser.display_name();
        // Beta and Dev builds of one browser share a display word, so the
        // same name can appear twice among the installed ones; one row each
        // would show two "Chrome" lines.
        if out.iter().any(|row| row.name.eq_ignore_ascii_case(&name)) {
            continue;
        }
        out.push(BrowserRow {
            key: name.to_ascii_lowercase(),
            hidden: is_hidden(&name),
            family: family_name(browser.family),
            state: "not_connected",
            client_ids: Vec::new(),
            name,
        });
    }

    for client in paired {
        let word = browser_word(&client.label);
        // The row this pairing belongs to: by the table, so "Chrome" lands
        // on "Chrome" and not on a differently-spelled duplicate.
        let (name, family) = BROWSERS
            .iter()
            .find(|b| b.matches_name(word))
            .map(|b| (b.display_name(), family_name(b.family)))
            .unwrap_or_else(|| (word.to_owned(), "chromium"));

        let index = match out.iter().position(|row| row.name.eq_ignore_ascii_case(&name)) {
            Some(index) => index,
            None => {
                out.push(BrowserRow {
                    key: name.to_ascii_lowercase(),
                    hidden: is_hidden(&name),
                    family,
                    state: "not_connected",
                    client_ids: Vec::new(),
                    name,
                });
                out.len() - 1
            }
        };
        out[index].client_ids.push(client.client_id.clone());
    }

    // State last, once every pairing is attached. Connected if any of its
    // profiles is on; off only when every one of them is off.
    for row in &mut out {
        let mine: Vec<&idm_ipc::PairedClient> =
            paired.iter().filter(|c| row.client_ids.contains(&c.client_id)).collect();
        row.state = if mine.is_empty() {
            "not_connected"
        } else if mine.iter().any(|c| c.enabled) {
            "connected"
        } else {
            "off"
        };
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(label: &str) -> Browser {
        Browser {
            label: Box::leak(label.to_owned().into_boxed_str()),
            root: "x",
            family: Family::Chromium,
        }
    }

    #[test]
    fn the_extensions_one_word_names_find_their_rows() {
        // These are the words the extension puts in its pairing label.
        for (word, row) in [
            ("Chrome", "Google Chrome"),
            ("Edge", "Microsoft Edge"),
            ("Brave", "Brave"),
            ("Firefox", "Firefox (and forks)"),
            ("Opera", "Opera"),
        ] {
            assert!(named(row).matches_name(word), "{word} should match {row}");
        }
    }

    #[test]
    fn a_name_does_not_match_an_unrelated_row() {
        assert!(!named("Brave").matches_name("Chrome"));
        assert!(!named("Google Chrome").matches_name("Firefox"));
        assert!(!named("Brave").matches_name(""), "an empty name matches nothing");
    }

    #[test]
    fn display_names_drop_the_vendor_and_the_explanation() {
        assert_eq!(named("Google Chrome").display_name(), "Chrome");
        assert_eq!(named("Microsoft Edge").display_name(), "Edge");
        assert_eq!(named("Firefox (and forks)").display_name(), "Firefox");
        assert_eq!(named("Brave").display_name(), "Brave");
    }

    #[test]
    fn the_first_chrome_row_is_the_real_one() {
        // "Chrome" also appears in Beta, Dev and Canary; the pairing of the
        // ordinary browser has to land on the ordinary row.
        let first = BROWSERS.iter().find(|b| b.matches_name("Chrome")).unwrap();
        assert_eq!(first.label, "Google Chrome");
    }

    #[test]
    fn every_installed_browser_comes_from_the_table() {
        for browser in installed() {
            assert!(BROWSERS.iter().any(|b| b.root == browser.root));
        }
    }

    fn client(id: &str, label: &str, enabled: bool) -> idm_ipc::PairedClient {
        idm_ipc::PairedClient {
            client_id: id.into(),
            label: label.into(),
            paired_at_unix: 1,
            enabled,
        }
    }

    fn pick(labels: &[&str]) -> Vec<Browser> {
        labels
            .iter()
            .map(|l| *BROWSERS.iter().find(|b| b.label == *l).expect("a table entry"))
            .collect()
    }

    #[test]
    fn a_paired_browser_is_connected_and_an_unpaired_one_is_not() {
        let installed = pick(&["Google Chrome", "Firefox (and forks)"]);
        let paired = [client("a", "Chrome on this PC", true)];
        let rows = rows(&installed, &paired, &[]);

        assert_eq!(rows[0].name, "Chrome");
        assert_eq!(rows[0].state, "connected");
        assert_eq!(rows[1].name, "Firefox");
        assert_eq!(rows[1].state, "not_connected");
        assert_eq!(rows[1].family, "gecko");
    }

    #[test]
    fn a_switched_off_browser_is_off_not_unconnected() {
        // Off is one switch from working. Calling it unconnected would send
        // the user to reinstall an extension that is perfectly fine.
        let installed = pick(&["Google Chrome"]);
        let rows = rows(&installed, &[client("a", "Chrome on this PC", false)], &[]);
        assert_eq!(rows[0].state, "off");
        assert_eq!(rows[0].client_ids, vec!["a".to_owned()]);
    }

    #[test]
    fn two_profiles_of_one_browser_share_a_row() {
        let installed = pick(&["Google Chrome"]);
        let paired =
            [client("work", "Chrome on this PC", true), client("home", "Chrome on this PC", false)];
        let rows = rows(&installed, &paired, &[]);
        assert_eq!(rows.len(), 1, "one browser, one line");
        assert_eq!(rows[0].client_ids.len(), 2);
        assert_eq!(rows[0].state, "connected", "on if any profile is on");
    }

    #[test]
    fn a_paired_browser_that_is_not_detected_still_shows() {
        // A working connection must not vanish because a registry key is
        // missing -- a portable copy, say.
        let rows = rows(&[], &[client("a", "Brave on this PC", true)], &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "Brave");
        assert_eq!(rows[0].state, "connected");
    }

    #[test]
    fn a_label_the_table_does_not_know_still_gets_a_row() {
        let rows = rows(&[], &[client("a", "Zorblax on this PC", true)], &[]);
        assert_eq!(rows[0].name, "Zorblax");
        assert_eq!(rows[0].state, "connected");
    }

    #[test]
    fn hiding_flags_a_row_without_removing_it() {
        // Dropped rows could not be offered back.
        let installed = pick(&["Google Chrome", "Firefox (and forks)"]);
        let rows = rows(&installed, &[], &["firefox".to_owned()]);
        assert_eq!(rows.len(), 2, "hidden browsers are still listed");
        assert!(!rows[0].hidden);
        assert!(rows[1].hidden, "matched without regard to case");
    }

    #[test]
    fn hiding_a_connected_browser_does_not_disconnect_it() {
        let installed = pick(&["Google Chrome"]);
        let rows = rows(&installed, &[client("a", "Chrome on this PC", true)], &["Chrome".into()]);
        assert!(rows[0].hidden);
        assert_eq!(rows[0].state, "connected", "hidden is about the view, not the connection");
    }

    #[test]
    fn beta_builds_do_not_make_a_second_chrome_line() {
        let installed = pick(&["Google Chrome", "Chrome Beta"]);
        let rows = rows(&installed, &[], &[]);
        assert_eq!(
            rows.iter().filter(|r| r.name == "Chrome").count(),
            1,
            "{:?}",
            rows.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
    }
}
