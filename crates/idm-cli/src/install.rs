//! Registering the native messaging host with whatever browsers are present.
//!
//! # Why this is a table and not four hardcoded paths
//!
//! There are only two extension codebases possible on Windows: one Chromium
//! build and one Gecko build. Every Chromium browser speaks the same
//! extension API and the same native messaging protocol, and so does every
//! Firefox fork. The *only* thing that differs per browser is the registry
//! key the host manifest is registered under.
//!
//! So supporting "any browser" is not N implementations, it is one table and
//! a loop. A browser missing from the table can still be added by hand with
//! `--extra-root`, and nothing here assumes a fixed set.
//!
//! Safari is the genuine exclusion: it is macOS-only and uses a different
//! extension model entirely.
//!
//! # The two manifest dialects
//!
//! Chromium lists permitted callers under `allowed_origins`, as
//! `chrome-extension://<id>/`. Gecko uses `allowed_extensions` with a plain
//! extension id. Same file otherwise, so both are generated from one place.
//! That list is the browser-enforced restriction on who may launch the host,
//! which is why it is never left empty.

use std::path::{Path, PathBuf};

/// The host name both the manifest and the registry key use.
pub const HOST_NAME: &str = "com.divine.idm";

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

/// Who may launch the host. Enforced by the browser, not by us.
#[derive(Debug, Clone)]
pub struct AllowedCallers {
    /// Chromium extension ids (32 lowercase letters).
    pub chromium_ids: Vec<String>,
    /// Gecko extension ids, usually an email-like string.
    pub gecko_ids: Vec<String>,
}

impl Default for AllowedCallers {
    fn default() -> Self {
        Self { chromium_ids: Vec::new(), gecko_ids: vec!["idm@divine.local".to_owned()] }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("registry: {0}")]
    Registry(String),
    #[error("no extension ids were given, which would let any extension launch the host")]
    NoCallers,
    /// Only reachable from the non-Windows stub, which exists so the logic
    /// above stays compilable and testable off Windows.
    #[cfg_attr(windows, allow(dead_code))]
    #[error("this installer is Windows-only")]
    Unsupported,
}

/// Builds the manifest JSON for one family.
pub fn manifest_json(
    family: Family,
    host_path: &Path,
    callers: &AllowedCallers,
) -> Result<String, InstallError> {
    // An empty list would be a manifest that any extension could use, which
    // removes the browser-side restriction entirely.
    let entries: Vec<String> = match family {
        Family::Chromium => {
            callers.chromium_ids.iter().map(|id| format!("\"chrome-extension://{id}/\"")).collect()
        }
        Family::Gecko => callers.gecko_ids.iter().map(|id| format!("\"{id}\"")).collect(),
    };
    if entries.is_empty() {
        return Err(InstallError::NoCallers);
    }

    let key = match family {
        Family::Chromium => "allowed_origins",
        Family::Gecko => "allowed_extensions",
    };
    // JSON needs the backslashes in a Windows path escaped.
    let escaped = host_path.display().to_string().replace('\\', "\\\\");
    Ok(format!(
        "{{\n  \"name\": \"{HOST_NAME}\",\n  \"description\": \"Divine IDM native messaging host\",\n  \"path\": \"{escaped}\",\n  \"type\": \"stdio\",\n  \"{key}\": [{}]\n}}\n",
        entries.join(", ")
    ))
}

/// Where the generated manifests are written.
pub fn manifest_dir() -> PathBuf {
    idm_ipc::endpoint_path().with_file_name("hosts")
}

pub fn manifest_path(family: Family) -> PathBuf {
    let leaf = match family {
        Family::Chromium => "com.divine.idm.chromium.json",
        Family::Gecko => "com.divine.idm.gecko.json",
    };
    manifest_dir().join(leaf)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registered {
    pub label: String,
    pub detected: bool,
}

#[derive(Debug, Default)]
pub struct InstallReport {
    pub registered: Vec<Registered>,
    pub skipped: Vec<String>,
    pub failures: Vec<(String, String)>,
}

/// Writes the manifests and registers them with every browser found.
///
/// `all` registers with every known browser rather than only the installed
/// ones, so a browser installed later still finds the host.
pub fn install(
    host_path: &Path,
    callers: &AllowedCallers,
    all: bool,
    extra_roots: &[(String, Family)],
) -> Result<InstallReport, InstallError> {
    if !host_path.is_file() {
        return Err(InstallError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("host binary not found at {}", host_path.display()),
        )));
    }

    std::fs::create_dir_all(manifest_dir())?;
    for family in [Family::Chromium, Family::Gecko] {
        let json = manifest_json(family, host_path, callers)?;
        std::fs::write(manifest_path(family), json)?;
    }

    let mut report = InstallReport::default();
    let extras: Vec<Browser> = extra_roots
        .iter()
        .map(|(root, family)| Browser {
            // Leaked deliberately: the table is static and these live for the
            // process, which ends moments later.
            label: Box::leak(root.clone().into_boxed_str()),
            root: Box::leak(root.clone().into_boxed_str()),
            family: *family,
        })
        .collect();

    for browser in BROWSERS.iter().chain(extras.iter()) {
        let detected = platform::browser_present(browser.root);
        if !detected && !all {
            report.skipped.push(browser.label.to_owned());
            continue;
        }
        match platform::register(browser.root, &manifest_path(browser.family)) {
            Ok(()) => {
                report.registered.push(Registered { label: browser.label.to_owned(), detected });
            }
            Err(error) => {
                report.failures.push((browser.label.to_owned(), error.to_string()));
            }
        }
    }
    Ok(report)
}

/// Removes the host registration from every known browser.
pub fn uninstall() -> InstallReport {
    let mut report = InstallReport::default();
    for browser in BROWSERS {
        match platform::unregister(browser.root) {
            Ok(true) => {
                report
                    .registered
                    .push(Registered { label: browser.label.to_owned(), detected: true });
            }
            Ok(false) => report.skipped.push(browser.label.to_owned()),
            Err(error) => report.failures.push((browser.label.to_owned(), error.to_string())),
        }
    }
    let _ = std::fs::remove_file(manifest_path(Family::Chromium));
    let _ = std::fs::remove_file(manifest_path(Family::Gecko));
    report
}

#[cfg(windows)]
mod platform {
    use super::InstallError;
    use std::path::Path;

    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE,
        REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegEnumKeyExW, RegOpenKeyExW,
        RegQueryInfoKeyW, RegSetValueExW,
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
    pub(super) fn browser_present(root: &str) -> bool {
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

    /// Points the browser at our manifest. Always HKCU: registering per-user
    /// needs no elevation and is the right scope for a per-user tool.
    pub(super) fn register(root: &str, manifest: &Path) -> Result<(), InstallError> {
        let path = wide(&format!(r"Software\{root}\NativeMessagingHosts\{}", super::HOST_NAME));
        let mut key = HKEY::default();
        // SAFETY: NUL-terminated inputs; `key` is a valid out-pointer.
        let status = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(path.as_ptr()),
                None,
                None,
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                None,
                &raw mut key,
                None,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(InstallError::Registry(format!("RegCreateKeyEx failed: {status:?}")));
        }

        // The default value holds the manifest path.
        let value = wide(&manifest.display().to_string());
        let bytes =
            unsafe { std::slice::from_raw_parts(value.as_ptr().cast::<u8>(), value.len() * 2) };
        // SAFETY: `bytes` borrows `value`, which outlives the call, and its
        // length matches exactly.
        let status = unsafe { RegSetValueExW(key, PCWSTR::null(), None, REG_SZ, Some(bytes)) };
        // SAFETY: created above, not used after this.
        unsafe {
            let _ = RegCloseKey(key);
        }
        if status != ERROR_SUCCESS {
            return Err(InstallError::Registry(format!("RegSetValueEx failed: {status:?}")));
        }
        Ok(())
    }

    /// Returns whether anything was actually removed.
    /// Removes a whole browser root. Tests only: a real uninstall leaves
    /// the browser's own keys alone and touches only our registration.
    #[cfg(test)]
    pub(super) fn delete_root(root: &str) {
        let path = wide(&format!(r"Software\{root}"));
        // SAFETY: `path` is a NUL-terminated wide string outliving the call.
        unsafe {
            let _ = RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr()));
        }
    }

    pub(super) fn unregister(root: &str) -> Result<bool, InstallError> {
        let path = wide(&format!(r"Software\{root}\NativeMessagingHosts\{}", super::HOST_NAME));
        // SAFETY: NUL-terminated wide string outliving the call.
        let status = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr())) };
        Ok(status == ERROR_SUCCESS)
    }
}

#[cfg(not(windows))]
mod platform {
    use super::InstallError;
    use std::path::Path;

    pub(super) fn browser_present(_root: &str) -> bool {
        false
    }

    pub(super) fn register(_root: &str, _manifest: &Path) -> Result<(), InstallError> {
        Err(InstallError::Unsupported)
    }

    pub(super) fn unregister(_root: &str) -> Result<bool, InstallError> {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The installer must register the id the extension actually has.
    ///
    /// Two files have to agree: the keypair under `extension/keys/` fixes
    /// the id Chromium derives, and `installer-hooks.nsh` passes an id to
    /// `--install-host` during installation. Running `keygen.mjs --force`
    /// changes the first and not the second.
    ///
    /// That drift fails silently in the worst way. The install succeeds, the
    /// registry key is written, the manifest looks right -- and the browser
    /// refuses to launch the host, because the extension presenting itself
    /// is not the one `allowed_origins` names. Nothing logs it, and the only
    /// symptom is that downloads quietly keep going to the browser.
    ///
    /// `make-installer.ps1` checks this too, but only when an installer is
    /// being built. Here it is checked whenever anyone runs the tests.
    #[test]
    fn the_installer_registers_the_id_the_extension_actually_has() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");

        let pinned = std::fs::read_to_string(root.join("extension/keys/chromium.pub.txt"))
            .expect("extension/keys/chromium.pub.txt is missing; run node extension/keygen.mjs");
        let pinned = pinned.lines().next().unwrap_or_default().trim().to_owned();
        assert_eq!(pinned.len(), 32, "a Chromium id is 32 characters: {pinned:?}");

        let hooks = std::fs::read_to_string(root.join("app/src-tauri/installer-hooks.nsh"))
            .expect("app/src-tauri/installer-hooks.nsh is missing");
        let declared = hooks
            .split_once("!define IDM_EXTENSION_ID")
            .and_then(|(_, rest)| rest.split('"').nth(1))
            .expect("installer-hooks.nsh does not define IDM_EXTENSION_ID");

        assert_eq!(
            declared, pinned,
            "the installer would register {declared}, but the extension's id is {pinned}.              Update IDM_EXTENSION_ID in app/src-tauri/installer-hooks.nsh."
        );
    }

    fn callers() -> AllowedCallers {
        AllowedCallers {
            chromium_ids: vec!["abcdefghijklmnopabcdefghijklmnop".into()],
            gecko_ids: vec!["idm@divine.local".into()],
        }
    }

    #[test]
    fn the_two_families_use_their_own_manifest_keys() {
        let path = Path::new(r"C:\Program Files\DivineIDM\idm-host.exe");

        let chromium = manifest_json(Family::Chromium, path, &callers()).unwrap();
        assert!(chromium.contains("allowed_origins"), "{chromium}");
        assert!(chromium.contains("chrome-extension://abcdefghijklmnopabcdefghijklmnop/"));
        assert!(!chromium.contains("allowed_extensions"));

        let gecko = manifest_json(Family::Gecko, path, &callers()).unwrap();
        assert!(gecko.contains("allowed_extensions"), "{gecko}");
        assert!(gecko.contains("idm@divine.local"));
        assert!(!gecko.contains("allowed_origins"));
    }

    #[test]
    fn a_manifest_with_no_permitted_callers_is_refused() {
        // An empty list means any extension could launch the host, which
        // removes the browser-side restriction entirely.
        let empty = AllowedCallers { chromium_ids: Vec::new(), gecko_ids: Vec::new() };
        let path = Path::new(r"C:\x\idm-host.exe");
        assert!(matches!(
            manifest_json(Family::Chromium, path, &empty),
            Err(InstallError::NoCallers)
        ));
        assert!(matches!(manifest_json(Family::Gecko, path, &empty), Err(InstallError::NoCallers)));
    }

    #[test]
    fn windows_paths_are_escaped_for_json() {
        let path = Path::new(r"C:\Program Files\DivineIDM\idm-host.exe");
        let json = manifest_json(Family::Chromium, path, &callers()).unwrap();
        assert!(json.contains(r"C:\\Program Files\\DivineIDM\\idm-host.exe"), "{json}");
        // And the result must actually parse.
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(parsed["path"], r"C:\Program Files\DivineIDM\idm-host.exe");
        assert_eq!(parsed["type"], "stdio");
        assert_eq!(parsed["name"], HOST_NAME);
    }

    #[test]
    fn every_known_browser_has_a_distinct_registry_root() {
        // A duplicate root would silently mean one browser overwrites
        // another's registration.
        let mut roots: Vec<&str> = BROWSERS.iter().map(|b| b.root).collect();
        let before = roots.len();
        roots.sort_unstable();
        roots.dedup();
        assert_eq!(before, roots.len(), "duplicate registry roots in the browser table");
    }

    #[test]
    fn the_table_covers_both_families_broadly() {
        let chromium = BROWSERS.iter().filter(|b| b.family == Family::Chromium).count();
        let gecko = BROWSERS.iter().filter(|b| b.family == Family::Gecko).count();
        // The point of the table is breadth; a handful of each is the minimum
        // that makes "any browser" a fair claim.
        assert!(chromium >= 10, "only {chromium} Chromium browsers listed");
        assert!(gecko >= 3, "only {gecko} Gecko browsers listed");
    }

    #[test]
    fn manifests_for_the_two_families_do_not_collide() {
        assert_ne!(manifest_path(Family::Chromium), manifest_path(Family::Gecko));
    }

    #[cfg(windows)]
    #[test]
    fn browser_detection_answers_without_panicking() {
        // Either answer is legitimate on any given machine; what matters is
        // that probing an absent browser is not an error.
        let present: Vec<&str> = BROWSERS
            .iter()
            .filter(|b| platform::browser_present(b.root))
            .map(|b| b.label)
            .collect();
        println!("detected browsers: {present:?}");
        assert!(!platform::browser_present(r"Nonexistent\Browser\XYZZY"));
    }

    #[cfg(windows)]
    #[test]
    fn registering_a_host_does_not_make_a_browser_look_installed() {
        // The bug this guards for: `register` creates
        // Software\<root>\NativeMessagingHosts, and so creates
        // Software\<root>. A detector that asks only whether that key
        // exists is reading evidence it planted itself, and reports every
        // browser it has ever registered as installed forever after. Found on
        // a machine where HKCU\Software\Vivaldi existed, held nothing but
        // NativeMessagingHosts, and no Vivaldi had ever been installed.
        const ROOT: &str = r"DivineIDMTest\PhantomBrowser";

        let dir = tempfile::tempdir().expect("temp dir");
        let manifest = dir.path().join("host.json");
        std::fs::write(&manifest, "{}").expect("write manifest");

        platform::delete_root(ROOT);
        assert!(!platform::browser_present(ROOT), "absent before the test writes anything");

        platform::register(ROOT, &manifest).expect("register");
        let after_our_own_registration = platform::browser_present(ROOT);

        // Stands in for a key the browser itself writes on first run.
        platform::register(&format!(r"{ROOT}\BLBeacon"), &manifest).expect("evidence");
        let with_browser_written_content = platform::browser_present(ROOT);

        platform::delete_root(ROOT);

        assert!(!after_our_own_registration, "our own registration is not evidence");
        assert!(with_browser_written_content, "a key the browser wrote is evidence");
    }
}
